//! The [`EqBackend`] implementation that drives **CAGEq's own APO** (filter.md §5.3c) — the
//! app-side half of the thing `cageq-apo/` is the audio-side half of.
//!
//! ## Two paths, deliberately
//! Every [`EqBackend::apply`] does two things, and they answer different questions:
//!
//! * **Writes the per-endpoint config file.** This is what makes the correction survive: the
//!   APO loads it at `LockForProcess`, so EQ is live from boot whether or not CAGEq is
//!   running — matching what EqualizerAPO does from `config.txt`, and the behaviour users
//!   already have.
//! * **Pushes coefficients down the control channel.** This is what makes editing feel live:
//!   the APO picks them up on its next buffer, with filter state carried and coefficients
//!   ramped, instead of waiting for a reload.
//!
//! Neither is a fallback for the other. Without the file the correction would exist only
//! while the app is open; without the channel every drag frame would be a disk round trip.
//! The channel is absent whenever no stream is running on that endpoint — an ordinary state,
//! not an error, and the file covers it.
//!
//! ## Why it depends on the APO crate
//! It renders the config with the APO's own writer and computes coefficients with the APO's
//! own `dsp::coefficients`. That is the point: the two halves cannot drift into disagreeing
//! about the file format or the filter math, because there is only one of each.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use cageq_apo::channel::ControlChannel;
use cageq_apo::config::{self, ApoConfig};
use cageq_apo::control::RawCoeffs;
use cageq_apo::dsp::{self, Band, FilterKind};
use cageq_backend::{
    BackendError, Capabilities, DeviceConfig, EqBackend, Filter, FilterType, StartupDecision,
};
use sha2::{Digest, Sha256};

/// CAGEq's own APO, as registered. Must match `CLSID_CageqApo` in `cageq-apo/shim/` and
/// `scripts/register.ps1` — if these three drift, `drives_endpoint` starts lying.
const CAGEQ_APO_CLSID: &str = "{530052E1-2CD4-400A-AC2B-0D19273AD5B7}";

/// Marker line carrying the content hash, written into each config file.
///
/// A comment, so the APO's parser skips it — the format already ignores `#` lines, and this
/// deliberately does not become a directive the audio side has to understand. It exists only
/// for the §3.0 startup-integrity check.
const HASH_PREFIX: &str = "# cageq-hash=";

#[derive(Debug, thiserror::Error)]
pub enum ApoBackendError {
    #[error("'{0}' is not an endpoint GUID")]
    BadEndpointId(String),
    #[error("could not write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The APO has not published a sample rate, so coefficients cannot be computed for it.
    /// Only reachable with a channel open, which means an APO instance is locked — so this
    /// is a version mismatch between app and APO, not a normal state.
    #[error("the APO on {0} published no sample rate; is CAGEqApo.dll current?")]
    NoSampleRate(String),
    /// The live control channel refused the correction outright — too many bands
    /// (`dsp::MAX_BANDS`), an unsafe/unstable coefficient, or an out-of-range preamp (see
    /// `control::publish`'s own checks). The config *file* still wrote (checked strictly
    /// before this), so the correction is durably saved and will be retried at the endpoint's
    /// next `LockForProcess` — where it will be refused there too, for the same reason,
    /// equally invisibly (that boundary can't report back to the app at all, see
    /// `cageq_apo_load_config`'s own doc). Surfacing it now, while there's still a channel
    /// open to tell a refusal apart from "nothing playing," is the only chance the app gets.
    #[error("the correction for {0} was refused by CAGEq's own engine (too many bands, or an unsafe filter) — saved to disk, but not live")]
    LivePushRefused(String),
}

/// Applies filters through CAGEq's own APO. Holds the directory corrections live in;
/// everything else is derived from an endpoint id.
///
/// `slots`: per-endpoint live-push slot assignment (§5.3c "identity-matched ramps") — see
/// [`SlotAssignment`]'s own doc for what it's for. `Mutex`, not `RefCell`: [`EqBackend::apply`]
/// takes `&self`, so this is the interior mutability that needs, and the app already shares one
/// `CageqApoBackend` behind an `Arc` rather than cloning it — nothing actually needed `Clone`,
/// which a bare `Mutex` field can't derive anyway.
#[derive(Debug)]
pub struct CageqApoBackend {
    config_dir: PathBuf,
    slots: std::sync::Mutex<std::collections::HashMap<String, SlotAssignment>>,
}

impl Default for CageqApoBackend {
    fn default() -> Self {
        CageqApoBackend::new(config::config_dir())
    }
}

impl CageqApoBackend {
    /// Target `config_dir` — normally `%ProgramData%\CAGEq\apo` via [`Default`]. Injectable
    /// so tests can drive a temp directory rather than the machine's real configuration.
    pub fn new(config_dir: impl Into<PathBuf>) -> Self {
        CageqApoBackend { config_dir: config_dir.into(), slots: std::sync::Mutex::new(std::collections::HashMap::new()) }
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    /// Every config file currently in the directory, sorted by name.
    ///
    /// Sorted because the aggregate hash is computed over them in order and must not depend
    /// on how the filesystem happens to enumerate. Unreadable directory reads as empty, which
    /// is the same as "nothing applied".
    fn config_files(&self) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(&self.config_dir) else {
            return Vec::new();
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "cfg"))
            .collect();
        paths.sort();
        paths
    }

    /// Hash of everything currently applied, across every endpoint.
    ///
    /// One hash for many files, because settings.json remembers exactly one per §3.0. Built
    /// from filename *and* content so that adding, removing or renaming a device's correction
    /// all register as a change — content alone would miss a file being deleted when another
    /// identical one exists.
    fn applied_hash(&self) -> String {
        let mut hasher = Sha256::new();
        for path in self.config_files() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                hasher.update(name.as_bytes());
            }
            if let Ok(body) = fs::read_to_string(&path) {
                hasher.update(strip_hash_line(&body).as_bytes());
            }
        }
        short_hex(&hasher.finalize())
    }

    /// Write one endpoint's correction, and push it live if the APO is running there.
    fn apply_one(&self, cfg: &DeviceConfig) -> Result<(), ApoBackendError> {
        let id = config::normalize_endpoint_id(&cfg.device)
            .ok_or_else(|| ApoBackendError::BadEndpointId(cfg.device.clone()))?;
        let apo_config = to_apo_config(cfg);

        self.write_config(&id, &apo_config)?;
        // No channel simply means nothing is playing on that endpoint — the file above is the
        // whole job in that case, and the APO will read it when it next locks. A genuine
        // refusal is different: the file is saved either way, but the live engine has
        // explicitly declined to run this correction (see `LivePushRefused`'s own doc for why
        // that's worth surfacing rather than swallowing — this used to be indistinguishable
        // from the benign "no channel" case, both discarded the same way, silently).
        match self.push_live(&id, &apo_config)? {
            PushOutcome::NoChannel | PushOutcome::Published => Ok(()),
            PushOutcome::Refused => Err(ApoBackendError::LivePushRefused(id)),
        }
    }

    fn write_config(&self, id: &str, apo_config: &ApoConfig) -> Result<(), ApoBackendError> {
        let path = config::config_path_in(&self.config_dir, id);
        let body = config::render(apo_config);
        let text = format!("{HASH_PREFIX}{}\n{body}", short_hex(&Sha256::digest(body.as_bytes())));
        write_atomic(&path, &text)
    }

    /// Push a correction down the live control channel, if one is open.
    ///
    /// The endpoint's sample rate comes from the APO rather than being assumed: coefficients
    /// depend on it, and a correction computed for the wrong rate lands at the wrong
    /// frequencies while looking entirely healthy.
    ///
    /// A method (not a free function) so it can reach `self.slots` — see [`SlotAssignment`]'s
    /// own doc for why the ramp needs it: `Cascade::start_ramp` pairs `start[i]`/`target[i]`
    /// purely by array position, and reordering `cfg.bands` into a *stable* position — the same
    /// band keeping the same slot across an edit — has to happen here, before it is flattened
    /// into the raw coefficients the channel actually carries, because only here does a band
    /// still have the identity (kind/Fc/Q) needed to recognise it as "the same band" at all.
    fn push_live(&self, endpoint_id: &str, cfg: &ApoConfig) -> Result<PushOutcome, ApoBackendError> {
        let Ok(channel) = ControlChannel::open(endpoint_id) else {
            return Ok(PushOutcome::NoChannel);
        };
        let Some(rate) = cageq_apo::control::sample_rate(channel.block()) else {
            return Err(ApoBackendError::NoSampleRate(endpoint_id.to_string()));
        };

        let assigned = {
            let mut slots = self.slots.lock().unwrap();
            slots.entry(endpoint_id.to_string()).or_default().assign(&cfg.bands)
        };
        let coeffs: Vec<RawCoeffs> = assigned
            .iter()
            .map(|slot| {
                let c = match slot {
                    Some(b) => dsp::coefficients(b, rate as f64),
                    // A slot whose band is gone this push — held here (not dropped from the
                    // array) so whatever is running in it fades to identity in place, the same
                    // "ramp toward PASSTHROUGH" `Cascade::start_ramp` already does when a
                    // correction merely gets *shorter*, just now happening mid-array instead of
                    // only at the tail.
                    None => dsp::Coeffs::PASSTHROUGH,
                };
                RawCoeffs { b0: c.b0, b1: c.b1, b2: c.b2, a1: c.a1, a2: c.a2 }
            })
            .collect();

        // The channel validates against the same limits the APO itself would, so a refusal here
        // means the correction was unsafe to run and the previous one is still in force — the
        // config file was written either way, which is the durable record of what the user
        // asked for, but the caller decides whether a refusal is worth surfacing (`apply_one`
        // does).
        Ok(if channel.publish(cfg.preamp_db, &coeffs) { PushOutcome::Published } else { PushOutcome::Refused })
    }
}

impl EqBackend for CageqApoBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // No reload to collide with: coefficients are pushed into a running filter, so
            // there is no minimum spacing to respect (§5.3a's MIN_WRITE_SPACING evaporates).
            min_write_spacing: std::time::Duration::ZERO,
            // The APO ramps coefficients in place with filter state carried, paced to the size
            // of the change (`ramp_ms_for` in dsp.rs, not the fixed 8 ms this comment used to
            // say — that was the floor for a live-drag-sized nudge even then), which the core
            // cannot better from outside. So the core must NOT emulate a transition by writing
            // intermediate frames — that would be two transitions fighting.
            owns_transitions: true,
            // Nothing else writes these files; there is no shared config surface to collide
            // over, which is one of the things dropping the EqAPO dependency buys.
            manages_foreign_config: false,
        }
    }

    fn apply(&self, configs: &[DeviceConfig]) -> Result<String, BackendError> {
        fs::create_dir_all(&self.config_dir)
            .map_err(|e| ApoBackendError::Write { path: self.config_dir.clone(), source: e })
            .map_err(BackendError::backend)?;

        for cfg in configs {
            self.apply_one(cfg).map_err(BackendError::backend)?;
        }
        Ok(self.applied_hash())
    }

    fn write_safe_state(&self) -> Result<(), BackendError> {
        // Silences every endpoint CAGEq has a correction for — which is exactly the set it
        // can affect, so enumerating audio devices (which can fail) is not needed on a path
        // that has to be as close to infallible as possible.
        //
        // Both halves matter: the file makes the safe state survive a restart, and the live
        // push makes it take effect *now*, which is the entire point of a watchdog fail-safe.
        // Waiting for the next `LockForProcess` would be no fail-safe at all.
        let safe = safe_state();
        for path in self.config_files() {
            let Some(id) = path.file_stem().and_then(|s| s.to_str()) else { continue };
            self.write_config(id, &safe).map_err(BackendError::backend)?;
            let _ = self.push_live(id, &safe);
        }
        Ok(())
    }

    fn startup_decision(&self, expected_hash: Option<&str>) -> Result<StartupDecision, BackendError> {
        let files = self.config_files();
        if files.is_empty() {
            return Ok(StartupDecision::FirstRun);
        }
        let actual = self.applied_hash();
        if expected_hash == Some(actual.as_str()) {
            return Ok(StartupDecision::ResumeTrusted);
        }
        // Every configured endpoint silenced means the §7.2 shutdown was active at exit.
        // Checked by *content* rather than by a remembered hash, because the safe-state
        // writer deliberately does not update settings.json's resume hash — there would be
        // nothing to resume to if it did.
        let all_safe = files.iter().all(|p| {
            fs::read_to_string(p)
                .ok()
                .and_then(|t| config::parse(strip_hash_line(&t).as_str()).ok())
                .is_some_and(is_safe_state)
        });
        Ok(if all_safe {
            StartupDecision::SafeStateStillActive
        } else {
            StartupDecision::ExternallyModified
        })
    }

    fn drives_endpoint(&self, device_id: &str) -> bool {
        cageq_backend::endpoint_has_apo(device_id, &[CAGEQ_APO_CLSID])
    }

    fn location(&self) -> String {
        self.config_dir.display().to_string()
    }

    fn applied_text(&self) -> Option<String> {
        // Re-read from disk rather than returning what was rendered: this is the UI's
        // end-to-end proof that the write landed, so it has to observe the files.
        let mut out = String::new();
        for path in self.config_files() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
            let _ = writeln!(out, "# {name}");
            out.push_str(&fs::read_to_string(&path).unwrap_or_default());
            out.push('\n');
        }
        Some(out)
    }
}

// ---------------------------------------------------------------------------
// Free helpers — pure where possible, so the decisions are testable without a filesystem.
// ---------------------------------------------------------------------------

/// CAGEq's safe state (§7.1/7.2): effective silence, no filters.
///
/// −120 dB rather than "no correction", because the neutral state of this APO is a
/// *passthrough* — full-volume audio. A safe state that removed the correction would make a
/// runaway louder, not quieter.
pub fn safe_state() -> ApoConfig {
    ApoConfig { preamp_db: -120.0, bands: Vec::new() }
}

fn is_safe_state(cfg: ApoConfig) -> bool {
    cfg.bands.is_empty() && cfg.preamp_db <= -119.9
}

/// Translate the neutral domain type into the APO's own.
pub fn to_apo_config(cfg: &DeviceConfig) -> ApoConfig {
    ApoConfig {
        preamp_db: cfg.preamp_db,
        bands: cfg.filters.iter().map(to_band).collect(),
    }
}

fn to_band(f: &Filter) -> Band {
    Band {
        kind: match f.kind {
            FilterType::Peaking => FilterKind::Peaking,
            FilterType::LowShelf => FilterKind::LowShelf,
            FilterType::HighShelf => FilterKind::HighShelf,
            FilterType::Bandpass => FilterKind::Bandpass,
        },
        freq_hz: f.freq_hz,
        gain_db: f.gain_db,
        q: f.q,
    }
}

/// What happened when trying to push a correction down an endpoint's live control channel —
/// three states, not two: whoever calls `push_live` needs to tell "there was nothing to push
/// to" apart from "there was, and it said no," and a plain `bool` can't do that (see
/// `ApoBackendError::LivePushRefused`'s own doc for why the difference matters).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    /// No stream is running on that endpoint — ordinary, not a problem; the config file is
    /// the whole job in this case.
    NoChannel,
    /// A channel was open and the correction was accepted.
    Published,
    /// A channel was open, but the correction was refused (see `control::publish`'s checks —
    /// too many bands, an unsafe/unstable coefficient, or an out-of-range preamp). The
    /// previous correction is still running; nothing was left half-applied.
    Refused,
}

/// Identity of a band for [`SlotAssignment`] — same reasoning as `cageq-core::morph::key()`
/// (same type, quantised centre and Q means "the same band" for matching purposes), a second
/// copy rather than a shared one because it works over `cageq_apo::dsp::Band`, not
/// `cageq_backend::Filter` — the two crates don't share a type, the same tradeoff already made
/// for e.g. the K-weighting constants duplicated across the sidecar/`morph.rs`/`biquad.ts`.
/// Quantised (not compared as raw floats) so a value that round-tripped through JSON/config text
/// can't fail to match itself by a float-formatting ULP.
fn band_key(b: &Band) -> (u8, i64, i64) {
    let kind = match b.kind {
        FilterKind::LowShelf => 0,
        FilterKind::HighShelf => 1,
        FilterKind::Peaking => 2,
        FilterKind::Bandpass => 3,
    };
    (kind, (b.freq_hz * 1000.0).round() as i64, (b.q * 1000.0).round() as i64)
}

/// Stable per-endpoint slot assignment for the live control channel.
///
/// `Cascade::start_ramp` (dsp.rs) pairs `start[i]`/`target[i]` purely by array position — it has
/// no notion of band identity at all, and cannot: the raw coefficients the control channel
/// carries don't encode Fc/Q/kind, only the finished biquad numbers. Left alone, that means an
/// edit that changes *which* bands are present — toggling one off, adding one, loading a preset
/// — silently reindexes every band after the change point: band N's old slot now holds whatever
/// band ended up at position N in the new list, and the ramp interpolates unrelated bands into
/// each other. Reported live as an audible "phasing" artefact on ordinary edits, not just a full
/// slot swap — the coefficient path has none of the protection `cageq-core::morph::lerp_bands`
/// already has for the EqAPO path (matching by identity before interpolating).
///
/// This fixes it upstream of `Cascade` rather than inside it: identity only exists here, where
/// `cfg.bands` is still symbolic, so the reordering has to happen before it's ever flattened
/// into raw coefficients — see `push_live`'s own doc. `Cascade`/the control channel need no
/// changes at all: a slot whose band was dropped is represented as an explicit `None` (which
/// `push_live` turns into `Coeffs::PASSTHROUGH`) if something *later* in the array is still
/// live, exactly the same "fade toward identity" `start_ramp` already does for a correction that
/// merely gets shorter — this only extends where in the array that can happen, from "the tail"
/// to "anywhere".
#[derive(Debug, Default)]
struct SlotAssignment {
    /// Index i is the identity occupying that slot as of the last push, or `None` for a slot
    /// freed since (its band dropped, nothing new claimed it yet).
    slots: Vec<Option<(u8, i64, i64)>>,
}

impl SlotAssignment {
    /// Reorders `bands` into slot order: a band whose identity matches a previous slot stays in
    /// that exact slot; everything else (genuinely new, or a duplicate identity beyond the first
    /// match — rare, and not worth more bookkeeping to handle perfectly) takes the lowest slot
    /// freed by a dropped band, or a fresh one past the end. Trailing frees are trimmed (the
    /// existing count-shrink behaviour already covers those); a freed slot with something later
    /// still live stays as an explicit gap.
    fn assign<'a>(&mut self, bands: &'a [Band]) -> Vec<Option<&'a Band>> {
        let mut out: Vec<Option<&Band>> = vec![None; self.slots.len()];
        let mut used = vec![false; bands.len()];

        for (i, slot) in self.slots.iter().enumerate() {
            let Some(key) = slot else { continue };
            if let Some(j) = bands.iter().position(|b| band_key(b) == *key) {
                if !used[j] {
                    out[i] = Some(&bands[j]);
                    used[j] = true;
                }
            }
        }
        for (j, band) in bands.iter().enumerate() {
            if used[j] {
                continue;
            }
            match out.iter_mut().find(|s| s.is_none()) {
                Some(slot) => *slot = Some(band),
                None => out.push(Some(band)),
            }
        }
        while out.last().is_some_and(Option::is_none) {
            out.pop();
        }

        self.slots = out.iter().map(|b| b.map(band_key)).collect();
        out
    }
}

/// Drop the hash marker so the remainder is exactly what was hashed.
fn strip_hash_line(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with(HASH_PREFIX))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn short_hex(digest: &[u8]) -> String {
    let mut s = String::with_capacity(8);
    for b in &digest[..4] {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Write via a temp file and rename, so a crash mid-write cannot leave a half-written
/// correction for the APO to parse — it would be refused wholesale, but a torn file that
/// happened to parse would be worse.
fn write_atomic(path: &Path, text: &str) -> Result<(), ApoBackendError> {
    let tmp = path.with_extension("cfg.tmp");
    let fail = |source| ApoBackendError::Write { path: path.to_path_buf(), source };
    fs::write(&tmp, text.as_bytes()).map_err(fail)?;
    fs::rename(&tmp, path).map_err(fail)
}

#[cfg(test)]
mod tests {
    use super::*;

    const EP: &str = "{6cafe423-cde5-4ec1-a1e2-e3fcec778349}";
    const EP2: &str = "{11112222-3333-4444-5555-666677778888}";
    // Own ids for the live-channel tests below: `ControlChannel::create` opens a machine-wide
    // `Global\` named section keyed by endpoint id, and tests run on separate threads within
    // the same process — reusing EP/EP2 here would race whichever other test runs alongside.
    const EP_LIVE_REFUSED: &str = "{99990001-0001-0001-0001-000000000001}";
    const EP_LIVE_OK: &str = "{99990002-0002-0002-0002-000000000002}";

    /// A private directory per test, so these never touch the machine's real corrections.
    fn temp_backend(tag: &str) -> (CageqApoBackend, PathBuf) {
        let dir = std::env::temp_dir()
            .join(format!("cageq-apo-backend-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        (CageqApoBackend::new(&dir), dir)
    }

    fn device(id: &str, preamp: f64) -> DeviceConfig {
        DeviceConfig {
            device: id.to_string(),
            preamp_db: preamp,
            filters: vec![
                Filter { kind: FilterType::Peaking, freq_hz: 105.0, gain_db: 6.0, q: 0.7 },
                Filter { kind: FilterType::HighShelf, freq_hz: 8000.0, gain_db: -3.0, q: 0.7 },
            ],
        }
    }

    /// The end-to-end contract that matters: what this backend writes, the APO's own parser
    /// accepts, and it describes the correction that was asked for. These are the two halves
    /// of one feature living in separate processes, so a round trip through the real parser
    /// is the only thing that proves they agree.
    #[test]
    fn what_the_backend_writes_the_apo_can_read_back() {
        let (backend, dir) = temp_backend("roundtrip");
        backend.apply(&[device(EP, -9.5)]).unwrap();

        let path = config::config_path_in(&dir, EP);
        let text = fs::read_to_string(&path).expect("config file should exist");
        let parsed = config::parse(&text).expect("the APO's own parser must accept it");

        assert_eq!(parsed.preamp_db, -9.5);
        assert_eq!(parsed.bands.len(), 2);
        assert_eq!(parsed.bands[0].freq_hz, 105.0);
        assert_eq!(parsed.bands[0].gain_db, 6.0);
        assert_eq!(parsed.bands[1].kind, FilterKind::HighShelf);
    }

    /// The device id is normalised, so a bare GUID and a braced one address one endpoint
    /// rather than quietly producing two corrections that fight each other.
    #[test]
    fn endpoint_ids_are_normalised_and_bad_ones_refused() {
        let (backend, dir) = temp_backend("ids");
        backend.apply(&[device("6CAFE423-CDE5-4EC1-A1E2-E3FCEC778349", -3.0)]).unwrap();
        assert!(config::config_path_in(&dir, EP).exists(), "should have normalised to {EP}");
        assert_eq!(backend.config_files().len(), 1);

        assert!(backend.apply(&[device("not-a-guid", 0.0)]).is_err());
    }

    /// §3.0 startup integrity, across all four verdicts.
    #[test]
    fn startup_decisions_reflect_what_is_actually_applied() {
        let (backend, _dir) = temp_backend("startup");
        assert_eq!(backend.startup_decision(None).unwrap(), StartupDecision::FirstRun);

        let hash = backend.apply(&[device(EP, -6.0)]).unwrap();
        assert_eq!(
            backend.startup_decision(Some(&hash)).unwrap(),
            StartupDecision::ResumeTrusted,
        );

        // Something else changed the correction.
        assert_eq!(
            backend.startup_decision(Some("deadbeef")).unwrap(),
            StartupDecision::ExternallyModified,
        );

        // The watchdog silenced things and the app never got to write a resume hash.
        backend.write_safe_state().unwrap();
        assert_eq!(
            backend.startup_decision(Some(&hash)).unwrap(),
            StartupDecision::SafeStateStillActive,
        );
    }

    /// **The safe state must be silence, not the absence of a correction.**
    ///
    /// This APO's neutral state is a passthrough — full-volume audio. Deleting the config
    /// would therefore make a runaway *louder*, which is the opposite of a fail-safe. It has
    /// to write an actively silencing correction instead, and that file must still be one the
    /// APO will accept (a -120 dB preamp sits exactly on its lower bound).
    #[test]
    fn the_safe_state_actively_silences_rather_than_removing_the_correction() {
        let (backend, dir) = temp_backend("safe");
        backend.apply(&[device(EP, -6.0), device(EP2, -3.0)]).unwrap();
        backend.write_safe_state().unwrap();

        for id in [EP, EP2] {
            let text = fs::read_to_string(config::config_path_in(&dir, id)).unwrap();
            let parsed = config::parse(&text).expect("safe state must still parse");
            assert!(parsed.bands.is_empty(), "safe state should carry no filters");
            assert_eq!(parsed.preamp_db, -120.0, "safe state must silence, not neutralise");
        }
        assert_eq!(backend.config_files().len(), 2, "every endpoint must be covered");
    }

    /// The aggregate hash has to move whenever what is applied moves — including when a
    /// device is *removed*, which a content-only hash could miss.
    #[test]
    fn the_hash_tracks_every_change_including_removals() {
        let (backend, dir) = temp_backend("hash");
        let one = backend.apply(&[device(EP, -6.0)]).unwrap();
        let same = backend.apply(&[device(EP, -6.0)]).unwrap();
        assert_eq!(one, same, "an unchanged correction must hash the same");

        let changed = backend.apply(&[device(EP, -7.0)]).unwrap();
        assert_ne!(one, changed, "a changed preamp must change the hash");

        let two = backend.apply(&[device(EP, -7.0), device(EP2, -7.0)]).unwrap();
        assert_ne!(changed, two, "adding a device must change the hash");

        // Removing a device's file must not read as unchanged.
        fs::remove_file(config::config_path_in(&dir, EP2)).unwrap();
        assert_ne!(two, backend.applied_hash(), "a removed device must change the hash");
    }

    /// The capabilities are the whole reason this backend exists — if these are wrong the
    /// core will emulate transitions on top of the APO's own, and throttle writes it need not.
    #[test]
    fn capabilities_hand_transitions_to_the_apo() {
        let (backend, _dir) = temp_backend("caps");
        let caps = backend.capabilities();
        assert!(caps.owns_transitions, "the APO ramps coefficients itself");
        assert_eq!(caps.min_write_spacing, std::time::Duration::ZERO, "no reload to collide with");
        assert!(!caps.manages_foreign_config, "nothing else writes these files");
    }

    /// A correction with no filters is legitimate (flat, preamp only) and must not be
    /// mistaken for the safe state.
    #[test]
    fn a_flat_correction_is_not_the_safe_state() {
        let (backend, _dir) = temp_backend("flat");
        let flat = DeviceConfig { device: EP.to_string(), preamp_db: 0.0, filters: vec![] };
        let hash = backend.apply(&[flat]).unwrap();
        assert_eq!(
            backend.startup_decision(Some(&hash)).unwrap(),
            StartupDecision::ResumeTrusted,
        );
        assert_eq!(backend.startup_decision(Some("x")).unwrap(), StartupDecision::ExternallyModified);
    }

    /// **The bug this fixes.** A correction past `dsp::MAX_BANDS` used to write its config file
    /// (unconditionally — no cap at write time) and then have its live push silently discarded
    /// either way, `apply()` still returning `Ok`: the app told the user "applied" while the
    /// live audio never received it, and the *only* other place that would ever have refused it
    /// — the APO's own config-file parser, at its next `LockForProcess`, inside audiodg — cannot
    /// report anything back across that privilege boundary. `apply` must now surface this.
    ///
    /// Creating a `Global\` section needs `SeCreateGlobalPrivilege`, which a normal developer
    /// account does not hold — same as `cageq-apo/src/channel.rs`'s own
    /// `a_real_section_round_trips_a_published_update`, this skips rather than fails when it
    /// cannot create one. (In production the creator is audiodg, which does hold it; CAGEq
    /// itself only ever *opens* an existing section, never creates one.)
    #[test]
    fn apply_reports_a_refused_live_push_instead_of_swallowing_it() {
        let (backend, dir) = temp_backend("live-refused");
        let Some(channel) = ControlChannel::create(EP_LIVE_REFUSED) else {
            eprintln!("skipping: could not create a Global\\ section (needs SeCreateGlobalPrivilege)");
            return;
        };
        cageq_apo::control::set_sample_rate(channel.block(), 48_000);

        let too_many: Vec<Filter> = (0..dsp::MAX_BANDS + 1)
            .map(|i| Filter { kind: FilterType::Peaking, freq_hz: 100.0 + i as f64, gain_db: 1.0, q: 1.0 })
            .collect();
        let cfg = DeviceConfig { device: EP_LIVE_REFUSED.to_string(), preamp_db: -3.0, filters: too_many };

        let err = backend.apply(&[cfg]).expect_err("a channel that refuses the push must fail the apply");
        assert!(matches!(err, BackendError::Backend(_)), "should be a backend error, not e.g. a bad-args one");

        // The config file is still the durable record of what was asked for, exactly as before
        // this fix — only the live-push half of `apply_one` changed.
        let path = config::config_path_in(&dir, EP_LIVE_REFUSED);
        let text = fs::read_to_string(&path).expect("the file must still be written despite the refusal");
        assert!(text.contains("Filter"), "the (too-large) correction should still be on disk: {text}");
    }

    /// The positive case, for contrast with the test above: a channel that's open and a
    /// correction well within `MAX_BANDS` must still succeed exactly as before this fix.
    /// Same `SeCreateGlobalPrivilege` caveat as the test above — skips, doesn't fail.
    #[test]
    fn apply_still_succeeds_live_when_the_channel_accepts_it() {
        let (backend, _dir) = temp_backend("live-ok");
        let Some(channel) = ControlChannel::create(EP_LIVE_OK) else {
            eprintln!("skipping: could not create a Global\\ section (needs SeCreateGlobalPrivilege)");
            return;
        };
        cageq_apo::control::set_sample_rate(channel.block(), 48_000);

        backend.apply(&[device(EP_LIVE_OK, -6.0)]).expect("a safe, small correction must still apply");
    }

    fn band(freq_hz: f64, gain_db: f64, q: f64) -> Band {
        Band { kind: FilterKind::Peaking, freq_hz, gain_db, q }
    }

    /// The bug this whole type exists to fix: dropping a band in the *middle* of the list must
    /// not reindex the ones after it. A plain positional diff would put `high`'s coefficients
    /// where `mid` used to be; this must instead recognise `high` and keep it exactly where it
    /// was, leaving `mid`'s old slot an explicit gap.
    #[test]
    fn removing_a_middle_band_does_not_reindex_the_one_after_it() {
        let mut sa = SlotAssignment::default();
        let low = band(100.0, 3.0, 0.7);
        let mid = band(1000.0, -2.0, 1.0);
        let high = band(8000.0, 4.0, 0.7);

        let all_three = [low, mid, high];
        let first = sa.assign(&all_three);
        assert_eq!(first.len(), 3);
        assert_eq!(first[2].unwrap().freq_hz, 8000.0, "high starts in slot 2");

        // Drop mid. Only low and high remain, in that order — a naive positional rebuild would
        // put high at index 1.
        let low_high = [low, high];
        let second = sa.assign(&low_high);
        assert_eq!(second.len(), 3, "high's slot must stay put, not collapse the array");
        assert_eq!(second[0].unwrap().freq_hz, 100.0, "low keeps its slot");
        assert!(second[1].is_none(), "mid's old slot fades in place instead of being reused yet");
        assert_eq!(second[2].unwrap().freq_hz, 8000.0, "high must still be in slot 2, not slot 1");
    }

    /// A band edited in place (same identity, different gain — a live drag) must keep its slot:
    /// identity is `(kind, Fc, Q)`, not gain, so this is exactly the common "nudge one band"
    /// case the fix must leave alone.
    #[test]
    fn editing_a_bands_gain_keeps_its_own_slot() {
        let mut sa = SlotAssignment::default();
        let a = band(1000.0, 2.0, 1.0);
        let b = band(5000.0, -3.0, 0.7);
        sa.assign(&[a, b]);

        let a_louder = band(1000.0, 6.0, 1.0);
        let louder_pair = [a_louder, b];
        let second = sa.assign(&louder_pair);
        assert_eq!(second[0].unwrap().gain_db, 6.0, "the edited band stays in slot 0");
        assert_eq!(second[1].unwrap().freq_hz, 5000.0, "the untouched band stays in slot 1");
    }

    /// A genuinely new band takes the slot a dropped one freed, rather than growing the array
    /// forever — the array only grows when there is no free slot to reuse.
    #[test]
    fn a_new_band_reuses_a_freed_slot_before_growing() {
        let mut sa = SlotAssignment::default();
        let low = band(100.0, 3.0, 0.7);
        let mid = band(1000.0, -2.0, 1.0);
        sa.assign(&[low, mid]);
        sa.assign(&[low]); // drop mid, freeing slot 1

        let high = band(8000.0, 1.0, 1.0);
        let low_high = [low, high];
        let third = sa.assign(&low_high);
        assert_eq!(third.len(), 2, "the new band reused the freed slot instead of appending");
        assert_eq!(third[1].unwrap().freq_hz, 8000.0);
    }

    /// Trailing frees must still shrink the array — the existing "correction gets shorter"
    /// behaviour this type must not regress, only extend to the middle of the array too.
    #[test]
    fn a_trailing_drop_still_shrinks_the_array() {
        let mut sa = SlotAssignment::default();
        let low = band(100.0, 3.0, 0.7);
        let high = band(8000.0, 1.0, 1.0);
        sa.assign(&[low, high]);

        let low_only = [low];
        let second = sa.assign(&low_only);
        assert_eq!(second.len(), 1, "dropping the last band must shrink, not leave a trailing gap");
    }

    /// The very first assignment (nothing tracked yet) must reproduce the given order exactly —
    /// no behaviour change for the common "first correction after opening a channel" case.
    #[test]
    fn the_first_assignment_preserves_the_given_order() {
        let mut sa = SlotAssignment::default();
        let bands = [band(100.0, 3.0, 0.7), band(1000.0, -2.0, 1.0), band(8000.0, 1.0, 1.0)];
        let out = sa.assign(&bands);
        let freqs: Vec<f64> = out.iter().map(|b| b.unwrap().freq_hz).collect();
        assert_eq!(freqs, vec![100.0, 1000.0, 8000.0]);
    }
}

/// Is CAGEq's own APO attached to any playback endpoint on this machine?
///
/// The selection signal for [`CageqApoBackend`] versus the Equalizer APO one. Deliberately
/// asks "is it actually attached", not "is the DLL present": registering the APO against an
/// endpoint is an explicit, elevated install step, so a true here means somebody chose this,
/// and no user is switched between audio engines by an app update alone.
pub fn is_installed() -> bool {
    cageq_backend::list_render_devices()
        .iter()
        .any(|d| cageq_backend::endpoint_has_apo(&d.id, &[CAGEQ_APO_CLSID]))
}

pub mod setup;
