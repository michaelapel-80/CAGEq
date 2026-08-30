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
}

/// Applies filters through CAGEq's own APO. Holds the directory corrections live in;
/// everything else is derived from an endpoint id.
#[derive(Debug, Clone)]
pub struct CageqApoBackend {
    config_dir: PathBuf,
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
        CageqApoBackend { config_dir: config_dir.into() }
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
        // Best-effort by design: no channel simply means nothing is playing on that endpoint,
        // in which case the file above is the whole job and the APO will read it when it next
        // locks. A push failure must never fail an apply.
        let _ = push_live(&id, &apo_config);
        Ok(())
    }

    fn write_config(&self, id: &str, apo_config: &ApoConfig) -> Result<(), ApoBackendError> {
        let path = config::config_path_in(&self.config_dir, id);
        let body = config::render(apo_config);
        let text = format!("{HASH_PREFIX}{}\n{body}", short_hex(&Sha256::digest(body.as_bytes())));
        write_atomic(&path, &text)
    }
}

impl EqBackend for CageqApoBackend {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            // No reload to collide with: coefficients are pushed into a running filter, so
            // there is no minimum spacing to respect (§5.3a's MIN_WRITE_SPACING evaporates).
            min_write_spacing: std::time::Duration::ZERO,
            // The APO ramps coefficients in place with filter state carried, over 8 ms, which
            // the core cannot better from outside. So the core must NOT emulate a transition
            // by writing intermediate frames — that would be two transitions fighting.
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
            let _ = push_live(id, &safe);
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

/// Push a correction down the live control channel, if one is open.
///
/// `Ok(false)` when there is no channel — no stream is running on that endpoint, which is
/// ordinary and means the config file is the whole job.
///
/// The endpoint's sample rate comes from the APO rather than being assumed: coefficients
/// depend on it, and a correction computed for the wrong rate lands at the wrong frequencies
/// while looking entirely healthy.
pub fn push_live(endpoint_id: &str, cfg: &ApoConfig) -> Result<bool, ApoBackendError> {
    let Ok(channel) = ControlChannel::open(endpoint_id) else {
        return Ok(false);
    };
    let Some(rate) = cageq_apo::control::sample_rate(channel.block()) else {
        return Err(ApoBackendError::NoSampleRate(endpoint_id.to_string()));
    };

    let coeffs: Vec<RawCoeffs> = cfg
        .bands
        .iter()
        .map(|b| {
            let c = dsp::coefficients(b, rate as f64);
            RawCoeffs { b0: c.b0, b1: c.b1, b2: c.b2, a1: c.a1, a2: c.a2 }
        })
        .collect();

    // A refusal here is not an error to propagate: the channel validates against the same
    // limits the APO would, so this means the correction was unsafe to run and the previous
    // one is still in force. The config file was written either way, which is the durable
    // record of what the user asked for.
    Ok(channel.publish(cfg.preamp_db, &coeffs))
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
