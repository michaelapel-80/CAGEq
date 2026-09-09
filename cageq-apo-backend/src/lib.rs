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
    /// Only reachable with a channel open, which means an APO instance is locked. Almost always
    /// a race on the very first buffer after `LockForProcess` (the section exists the instant
    /// `CreateFileMappingW` returns, before the rest of lock setup — including `set_sample_rate`
    /// — has run). See `ControlBlock`'s own doc for why this field's offset is now guaranteed
    /// stable across a [`dsp::MAX_BANDS`] change, so a *stale build* is no longer a plausible
    /// cause of this specific symptom the way it used to be.
    #[error("the APO on {0} published no sample rate — try again shortly")]
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
        match self.push_live(&id, &apo_config, true)? {
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
    ///
    /// `allow_crossfade` exists for exactly one caller — [`Self::write_safe_state`] passes
    /// `false` — because its target (`safe_state()`) always has zero bands, and
    /// `Cascade::start_crossfade`'s secondary bank with zero bands never trips
    /// `Cascade::process`'s dual-bank mix at all (`process_count2 == 0` is the same condition
    /// that gates it off): a crossfade-routed safe-state push would silently do *nothing* to the
    /// running audio, exactly when the fail-safe's push most needs to land.
    fn push_live(
        &self,
        endpoint_id: &str,
        cfg: &ApoConfig,
        allow_crossfade: bool,
    ) -> Result<PushOutcome, ApoBackendError> {
        let Ok(channel) = ControlChannel::open(endpoint_id) else {
            return Ok(PushOutcome::NoChannel);
        };
        let Some(rate) = cageq_apo::control::sample_rate(channel.block()) else {
            return Err(ApoBackendError::NoSampleRate(endpoint_id.to_string()));
        };

        // §5.2 isolate is the one place `cfg.bands` is always exactly one `Bandpass` — see
        // `is_isolate_audition`'s own doc. A push that *crosses* that boundary (starting or
        // ending an isolate audition) is a wholesale replacement of the correction, which
        // `Cascade::start_ramp` cannot do safely: it pairs `start[i]`/`target[i]` purely by
        // slot, so several unrelated bands independently fading toward `PASSTHROUGH` on the
        // same shared clock can sum into a real spike (measured, on a real correction, at
        // +22 dB above both endpoints — see `cageq-apo::dsp`'s
        // `a_coefficient_ramp_spikes_on_this_real_correction`). `start_crossfade` sidesteps
        // that entirely; an in-place edit (an isolate drag's own continuation, or an ordinary
        // tone edit, or an A/B slot switch — deliberately unaffected, see that test's own doc)
        // still wants the ramp, so this only fires on the actual boundary crossing.
        //
        // Entering/leaving isn't the only boundary, though: a drag that jumps far enough
        // between throttled pushes that `slot_reusable` refuses to reuse the sweep's own slot
        // (see its own doc — over an octave, or a big enough Q change) opens a *fresh* slot for
        // the new position and leaves the old one an independent fading gap — the exact same
        // "unrelated bands sharing one ramp clock" shape as the boundary case, just entirely
        // inside an isolate session (`is_isolate` stays `true` throughout, so that check alone
        // misses it). `isolate_boundary_crossed` covers both — driven by whether the isolate
        // band's *slot index itself* changed (`SlotAssignment::assign_isolate`), not by array
        // length: length alone missed the case where a drag swings back near an abandoned slot
        // (see that method's own doc for the real, reported bug this closes).
        let is_isolate = is_isolate_audition(&cfg.bands);
        let (assigned, crossfade) = {
            let mut slots = self.slots.lock().unwrap();
            let entry = slots.entry(endpoint_id.to_string()).or_default();
            let was_isolate = entry.was_isolate;
            let prev_isolate_slot = entry.isolate_slot;
            let out = entry.assign(&cfg.bands);
            let isolate_slot_changed = entry.isolate_slot != prev_isolate_slot;
            let crossfade =
                allow_crossfade && isolate_boundary_crossed(was_isolate, is_isolate, isolate_slot_changed);
            entry.was_isolate = is_isolate;
            (out, crossfade)
        };
        // Every push *within* an active isolate sweep (not itself a boundary crossing) is a
        // plain `apply_coeffs` call, same as any other in-place edit — `Cascade::start_ramp`
        // handles the timing on its own (see its own doc): a request arriving within
        // `GESTURE_GAP_MS` of the last one truncates to the `RAMP_MS` floor automatically, no
        // signal needed from here to say "this one's part of a fast gesture." A narrow, high-Q
        // isolate bandpass moving even a little used to read as a huge change on the plain
        // ramp's distance-scaled duration, so real drags (ticking roughly every 70 ms) kept
        // landing 100-300 ms ramps that never finished before the next tick replaced them — the
        // coefficients spent the whole gesture chasing the pointer instead of tracking it
        // (measured directly: up to ~18x behind — see `cageq-apo::dsp`'s
        // `set_bands_tracks_the_pointer_closely_through_a_real_isolate_drag`, replaying a real
        // captured drag).
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
        let published = channel.publish(cfg.preamp_db, &coeffs, crossfade);
        Ok(if published { PushOutcome::Published } else { PushOutcome::Refused })
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
            // `allow_crossfade: false` — see `push_live`'s own doc: `safe_state()` always has
            // zero bands, and a crossfade to zero bands silently does nothing to the running
            // audio, exactly when this fail-safe most needs to land.
            let _ = self.push_live(id, &safe, false);
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

/// Is `bands` the §5.2 isolate audition — a bandpass-only substitute for the real correction,
/// used to hear one filter's region in isolation? Always exactly one `Bandpass` band; anything
/// else (an ordinary correction, a shelf/peaking band alone, two bandpasses) is not.
fn is_isolate_audition(bands: &[Band]) -> bool {
    bands.len() == 1 && bands[0].kind == FilterKind::Bandpass
}

/// Should *this* push cross the wire as a crossfade (see `cageq_apo::control::ControlBlock`'s
/// `crossfade` field doc) rather than a plain ramp?
///
/// Two independent triggers, both the same underlying problem: an old band and a new,
/// unrelated one ending up on *different* slots that fade toward/from `PASSTHROUGH` on
/// `Cascade`'s one shared ramp clock.
///
///  1. **Entering or leaving isolate** (`is_isolate != was_isolate`) — the obvious case: the
///     whole correction is replaced by (or restored from) a single bandpass.
///  2. **`isolate_slot_changed` while *staying* inside isolate** — less obvious, easy to miss: a
///     drag that jumps far enough between throttled pushes that [`slot_reusable`] refuses to
///     hand the sweep its own slot back (over an octave, or too big a Q change — see that
///     function's own doc), *or* one that swings back near an earlier, already-abandoned slot
///     (see [`SlotAssignment::assign_isolate`]'s own doc for the exact reported bug this
///     catches — array length alone missed it), lands the band on a genuinely different slot
///     index than it held a moment ago. `is_isolate` stays `true` on both sides of that push,
///     so trigger 1 alone would miss it entirely — this session never "left" isolate, it just
///     briefly ran two unrelated bandpasses on two different, clock-sharing slots.
fn isolate_boundary_crossed(was_isolate: bool, is_isolate: bool, isolate_slot_changed: bool) -> bool {
    is_isolate != was_isolate || (is_isolate && isolate_slot_changed)
}

/// How far a genuinely new band's centre may sit from a freed slot's last known centre before
/// [`SlotAssignment::assign`] will reuse that slot for it, rather than opening a fresh one.
///
/// The identity match above only fires on an exact `band_key` hit; everything else used to be
/// "any free slot, any new band" — but a slot's coefficients don't teleport, `Cascade::start_ramp`
/// interpolates straight from whatever was last there to the new target. Reusing a slot that
/// was e.g. a 100 Hz band for an unrelated 8 kHz one makes that interpolation sweep audibly
/// through every octave in between, which is a worse artefact than the "phasing" this whole
/// mechanism exists to avoid, not a fix for it. One octave either way: close enough that a
/// direct ramp is a reasonable description of what happened to that region of the spectrum,
/// far enough to still catch same-band Q/gain-only edits (which don't move `Fc` at all).
const MAX_REUSE_FC_RATIO: f64 = 2.0;

/// Same idea as [`MAX_REUSE_FC_RATIO`], for `Q`. A slot that held a broad, gentle band and one
/// about to hold a narrow, sharp one (or vice versa) are not "the same band" just because their
/// centres happen to land close together — interpolating raw coefficients between two very
/// different bandwidths is audibly its own shape change, the same "sweep through everything in
/// between" artefact the Fc gate exists to avoid, just along the other axis a biquad has. Wider
/// than the Fc ratio (bandwidth varies more than Fc does across ordinary same-band edits) so an
/// in-place Q drag still reclaims its own slot.
const MAX_REUSE_Q_RATIO: f64 = 4.0;

/// Is `old` — a freed slot's last known identity, or `None` if the slot has never held one —
/// close enough to `band` for [`SlotAssignment::assign`] to reuse that slot? A slot that never
/// held anything is always fair game: there is no live interpolation to sweep away from.
///
/// Three independent gates, all of which must pass: same **filter kind**, [`MAX_REUSE_FC_RATIO`]
/// on `Fc`, and [`MAX_REUSE_Q_RATIO`] on `Q`. The kind check matters even when Fc and Q both look
/// close: a shelf and a peaking/bandpass band don't share a response *shape*, so interpolating
/// their raw coefficients is meaningless regardless of how near their numbers land — this is
/// exactly what let the §5.2 isolate sweep's narrow bandpass (always `Bandpass`, fixed `Q` — see
/// `SWEEP_Q` in `App.tsx`) reuse an existing shelf's freed slot whenever the sweep passed within
/// an octave of the shelf's own `Fc`, ramping straight from a shelf into a bandpass.
fn slot_reusable(old: &Option<(u8, i64, i64)>, band: &Band) -> bool {
    let Some((old_kind, old_freq_milli_hz, old_q_milli)) = old else { return true };
    let (kind, _, _) = band_key(band);
    if kind != *old_kind {
        return false;
    }
    let old_hz = *old_freq_milli_hz as f64 / 1000.0;
    if !(old_hz > 0.0 && band.freq_hz > 0.0) {
        return false;
    }
    let fc_ratio = (band.freq_hz / old_hz).max(old_hz / band.freq_hz);
    if fc_ratio > MAX_REUSE_FC_RATIO {
        return false;
    }
    let old_q = *old_q_milli as f64 / 1000.0;
    if !(old_q > 0.0 && band.q > 0.0) {
        return false;
    }
    let q_ratio = (band.q / old_q).max(old_q / band.q);
    q_ratio <= MAX_REUSE_Q_RATIO
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
/// into raw coefficients — see `push_live`'s own doc. A slot whose band was dropped is
/// represented as an explicit `None` (which `push_live` turns into `Coeffs::PASSTHROUGH`) if
/// something *later* in the array is still live, exactly the same "fade toward identity"
/// `start_ramp` already does for a correction that merely gets shorter — this only extends
/// where in the array that can happen, from "the tail" to "anywhere". (`Cascade` itself needed
/// no changes for *this* — reordering upstream is enough. Its later `start_crossfade` addition,
/// for the §5.2 isolate boundary specifically, is a separate mechanism `push_live` reaches for
/// on top of this reordering, not a change to it.)
///
/// A freed slot isn't fair game for just *any* new band, either — see [`slot_reusable`]. So a
/// slot's remembered identity survives rounds where nothing reused it (it stays fading toward
/// passthrough, untouched), rather than being wiped back to "empty" the moment its own band
/// drops; only [`MAX_BANDS`](dsp::MAX_BANDS)-headroom pays for that, not correctness.
#[derive(Debug, Default)]
struct SlotAssignment {
    /// Index i is the identity last assigned to that slot — kept even once that band is gone
    /// and the slot is fading toward passthrough, so a later unrelated band can't silently
    /// inherit its in-flight ramp (see [`slot_reusable`]). `None` only for a slot that has
    /// never held a band at all.
    slots: Vec<Option<(u8, i64, i64)>>,
    /// Whether the *previous* push to this endpoint was the §5.2 isolate audition (`cfg.bands`
    /// was exactly one `Bandpass`) — `push_live` compares this against the current push to
    /// decide whether the boundary is being crossed (crossfade) or not (plain ramp, as ever).
    was_isolate: bool,
    /// The slot index holding the §5.2 isolate audition band as of the *previous* push, if any
    /// — see [`Self::assign_isolate`]'s own doc for why this needs to be tracked explicitly
    /// rather than left to the generic search in [`Self::assign`].
    isolate_slot: Option<usize>,
}

impl SlotAssignment {
    /// Reorders `bands` into slot order — the §5.2 isolate audition (always exactly one
    /// `Bandpass`) is handled separately by [`Self::assign_isolate`]; this is the general path
    /// for everything else. A band whose identity matches a previous slot stays in that exact
    /// slot. Everything else (genuinely new, or a duplicate identity beyond the first match —
    /// rare, and not worth more bookkeeping to handle perfectly) reuses the lowest slot that is
    /// both free *and* [`slot_reusable`] with its last occupant, or opens a fresh one past the
    /// end if none qualifies. Trailing frees are trimmed (the existing count-shrink behaviour
    /// already covers those); a freed slot with something later still live stays as an explicit
    /// gap — and keeps remembering what it held, so it stays off-limits to a distant band for
    /// as long as it takes something close enough to come reclaim it.
    fn assign<'a>(&mut self, bands: &'a [Band]) -> Vec<Option<&'a Band>> {
        if let [band] = bands {
            if band.kind == FilterKind::Bandpass {
                return self.assign_isolate(band);
            }
        }
        // An ordinary push always ends any isolate session's slot tracking — a *later* isolate
        // press starts fresh rather than risking a stale index into a `self.slots` the generic
        // path below is about to reshuffle for reasons that have nothing to do with isolate.
        self.isolate_slot = None;

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
            let reuse_at = out
                .iter()
                .zip(self.slots.iter())
                .position(|(occupant, old)| occupant.is_none() && slot_reusable(old, band));
            match reuse_at {
                Some(i) => out[i] = Some(band),
                None => out.push(Some(band)),
            }
        }
        while out.last().is_some_and(Option::is_none) {
            out.pop();
        }

        // A slot nobody claimed this round keeps whatever identity it last remembered (still
        // fading, still off-limits to a distant band) instead of being wiped to "never used" —
        // only a slot that actually got a new occupant updates its memory.
        self.slots.resize(out.len(), None);
        for (i, occupant) in out.iter().enumerate() {
            if let Some(band) = occupant {
                self.slots[i] = Some(band_key(band));
            }
        }
        out
    }

    /// The §5.2 isolate half of [`Self::assign`], kept separate because isolate's invariant is
    /// different from an ordinary correction's: there is always at most **one** live isolate
    /// band, and a later push must always retarget the *same* slot the previous one landed in
    /// (if it is still [`slot_reusable`] compatible) — never fall back to the generic "any
    /// matching slot, lowest index wins" search in [`Self::assign`].
    ///
    /// **The bug this fixes, reported live**: a drag that swings back near an *earlier*,
    /// already-abandoned isolate position (having jumped elsewhere in between, opening a fresh
    /// slot each time it did) could have that generic search reclaim the OLD, abandoned slot —
    /// it still matches by Fc/Q, and sits at a lower index than the band's true current slot —
    /// instead of retargeting where the band actually, audibly is right now. `Cascade` then
    /// ramps the old, stale, already-mid-decay slot's coefficients toward the new target while
    /// the band's real current slot silently starts fading to `PASSTHROUGH` at a *different*
    /// index — an unpredictable jump, worse because it went unnoticed at the slot-assignment
    /// level: the array can *shrink* when this happens (the true current slot becomes the new
    /// trailing gap and gets trimmed), which the old array-length-based crossfade check
    /// (`out.len() > slots_before`) read as "nothing new happened" — the opposite of the truth.
    /// Reported as happening at "medium" drag speed, in either direction: exactly what it takes
    /// to leave several abandoned slots behind (each big-enough jump opens one) and then swing
    /// back near one of them, rather than either settling immediately (slow) or never revisiting
    /// old territory at all (one continuous fast sweep).
    ///
    /// Explicitly never searches any slot *other* than the tracked one: reusing any slot beyond
    /// it — even one that happens to match by Fc/Q — is exactly the class of bug this exists to
    /// close, not a case worth optimising for.
    fn assign_isolate<'a>(&mut self, band: &'a Band) -> Vec<Option<&'a Band>> {
        let mut out: Vec<Option<&Band>> = vec![None; self.slots.len()];

        let reuse_at = self
            .isolate_slot
            .filter(|&i| i < self.slots.len() && slot_reusable(&self.slots[i], band));
        match reuse_at {
            Some(i) => out[i] = Some(band),
            None => out.push(Some(band)),
        }
        while out.last().is_some_and(Option::is_none) {
            out.pop();
        }

        self.slots.resize(out.len(), None);
        self.isolate_slot = None;
        for (i, occupant) in out.iter().enumerate() {
            if let Some(b) = occupant {
                self.slots[i] = Some(band_key(b));
                self.isolate_slot = Some(i);
            }
        }
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

    /// **The bug this guards**: `write_safe_state`'s live push must never route through the
    /// crossfade path. Its target (`safe_state()`) always has zero bands, and
    /// `Cascade::start_crossfade`'s secondary bank with zero bands never trips
    /// `Cascade::process`'s dual-bank mix at all (`process_count2 == 0` is the same condition
    /// that gates it off) — a crossfade-routed safe-state push would silently do *nothing* to
    /// the running audio, exactly when the fail-safe's live push most needs to land. Reproduced
    /// directly: an endpoint whose last live push was a §5.2 isolate audition
    /// (`was_isolate == true`) crosses the isolate boundary on the very next push — which,
    /// without `allow_crossfade: false`, would be the safe-state push itself.
    #[test]
    fn write_safe_state_never_uses_the_crossfade_path() {
        let (backend, _dir) = temp_backend("safe-no-crossfade");
        let Some(channel) = ControlChannel::create(EP_LIVE_OK) else {
            eprintln!("skipping: could not create a Global\\ section (needs SeCreateGlobalPrivilege)");
            return;
        };
        cageq_apo::control::set_sample_rate(channel.block(), 48_000);

        // Put the endpoint into "last live push was isolate" state.
        let isolate = DeviceConfig {
            device: EP_LIVE_OK.to_string(),
            preamp_db: 0.0,
            filters: vec![Filter { kind: FilterType::Bandpass, freq_hz: 31.0, gain_db: 0.0, q: 8.0 }],
        };
        backend.apply(&[isolate]).unwrap();

        let mut snap = cageq_apo::control::Snapshot::default();
        assert!(
            matches!(cageq_apo::control::try_read(channel.block(), &mut snap), cageq_apo::control::ReadOutcome::Updated(_)),
            "sanity check: entering isolate must publish something readable"
        );
        assert!(snap.crossfade, "sanity check: entering isolate itself must still crossfade");

        backend.write_safe_state().unwrap();

        assert!(matches!(
            cageq_apo::control::try_read(channel.block(), &mut snap),
            cageq_apo::control::ReadOutcome::Updated(_)
        ));
        assert!(!snap.crossfade, "the safe-state push must never route through the crossfade path");
        assert_eq!(snap.band_count, 0, "the safe state itself must still be exactly zero bands");
    }

    /// End-to-end through the real wire: entering and leaving the §5.2 isolate audition must
    /// cross the wire as a crossfade; a continuation of the same drag (same slot, nearby
    /// frequency) and an ordinary edit must not. (What a continuation's ramp actually *does*
    /// with its timing — truncating to the `RAMP_MS` floor instead of chasing the
    /// distance-scaled duration — is `cageq_apo::dsp::Cascade::start_ramp`'s own concern now,
    /// decided entirely from its own recent-request history with no signal needed from here;
    /// see that method's own doc and `set_bands_tracks_the_pointer_closely_through_a_real_isolate_drag`.)
    #[test]
    fn isolate_boundary_crossings_cross_the_wire_as_a_crossfade() {
        const EP_ISOLATE_WIRE: &str = "{99990004-0004-0004-0004-000000000004}";
        let (backend, _dir) = temp_backend("isolate-boundary-wire");
        let Some(channel) = ControlChannel::create(EP_ISOLATE_WIRE) else {
            eprintln!("skipping: could not create a Global\\ section (needs SeCreateGlobalPrivilege)");
            return;
        };
        cageq_apo::control::set_sample_rate(channel.block(), 48_000);
        let mut snap = cageq_apo::control::Snapshot::default();

        let isolate_at = |freq_hz: f64| DeviceConfig {
            device: EP_ISOLATE_WIRE.to_string(),
            preamp_db: 0.0,
            filters: vec![Filter { kind: FilterType::Bandpass, freq_hz, gain_db: 0.0, q: 8.0 }],
        };

        // Entering the audition crosses the boundary.
        backend.apply(&[isolate_at(1000.0)]).unwrap();
        assert!(matches!(cageq_apo::control::try_read(channel.block(), &mut snap), cageq_apo::control::ReadOutcome::Updated(_)));
        assert!(snap.crossfade, "entering isolate must crossfade");

        // A continuation of the same drag (same slot, nearby frequency) must not.
        backend.apply(&[isolate_at(1050.0)]).unwrap();
        assert!(matches!(cageq_apo::control::try_read(channel.block(), &mut snap), cageq_apo::control::ReadOutcome::Updated(_)));
        assert!(!snap.crossfade, "an in-drag continuation must not crossfade");

        // Leaving the audition crosses the boundary again.
        backend.apply(&[device(EP_ISOLATE_WIRE, -6.0)]).unwrap();
        assert!(matches!(cageq_apo::control::try_read(channel.block(), &mut snap), cageq_apo::control::ReadOutcome::Updated(_)));
        assert!(snap.crossfade, "leaving isolate must crossfade");

        // An ordinary (non-isolate) edit must not.
        backend.apply(&[device(EP_ISOLATE_WIRE, -3.0)]).unwrap();
        assert!(matches!(cageq_apo::control::try_read(channel.block(), &mut snap), cageq_apo::control::ReadOutcome::Updated(_)));
        assert!(!snap.crossfade, "an ordinary edit must not crossfade");
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

    /// The decision `push_live` bases its crossfade-vs-ramp choice on: only a single, bare
    /// `Bandpass` counts. Anything else — empty, more than one band, a `Bandpass` alongside
    /// other bands (never produced by isolate's own write path, but must still not be
    /// misidentified if it ever arrived), or a single non-`Bandpass` band — is an ordinary
    /// correction and must ramp as it always has.
    #[test]
    fn only_a_single_bare_bandpass_counts_as_the_isolate_audition() {
        let bandpass = Band { kind: FilterKind::Bandpass, freq_hz: 100.0, gain_db: 0.0, q: 8.0 };
        assert!(is_isolate_audition(std::slice::from_ref(&bandpass)));

        assert!(!is_isolate_audition(&[]), "no bands at all is not isolate");
        assert!(!is_isolate_audition(std::slice::from_ref(&band(1000.0, 3.0, 1.0))), "an ordinary single band is not isolate");
        assert!(!is_isolate_audition(&[bandpass, band(1000.0, 3.0, 1.0)]), "a bandpass alongside other bands is not isolate");
        assert!(!is_isolate_audition(&[bandpass, bandpass]), "two bandpasses is not isolate either");
    }

    /// The two triggers `isolate_boundary_crossed` covers, and the two things it must leave
    /// alone: an ordinary edit or an A/B slot switch (`is_isolate` false on both sides) never
    /// crossfades regardless of `opened_new_slot` — that would re-litigate the already-made
    /// decision to keep those on the ramp (see `cageq-apo::dsp`'s
    /// `retuning_live_does_not_splatter...` for why).
    #[test]
    fn isolate_boundary_crossed_covers_entering_leaving_and_a_far_drag_jump() {
        // Entering: correction -> isolate.
        assert!(isolate_boundary_crossed(false, true, true), "entering isolate must crossfade");
        // Leaving: isolate -> correction.
        assert!(isolate_boundary_crossed(true, false, false), "leaving isolate must crossfade");
        // A drag continuing, but jumping far enough to open a fresh slot instead of reusing the
        // sweep's own one — the bug this test guards: `is_isolate` stays true throughout, so
        // only `opened_new_slot` distinguishes this from an ordinary in-place retune.
        assert!(
            isolate_boundary_crossed(true, true, true),
            "a far-enough drag jump mid-isolate must also crossfade, not just entering/leaving"
        );

        // A drag continuing with its own slot reused (the common, smooth case): no crossfade,
        // stay on the cheap ramp.
        assert!(!isolate_boundary_crossed(true, true, false), "an ordinary in-place drag retune must not crossfade");
        // Never isolate on either side, regardless of whether some *other* band's slot opened
        // fresh this round (an ordinary add-a-band edit, or an A/B switch) — out of scope here.
        assert!(!isolate_boundary_crossed(false, false, true), "a non-isolate edit must never crossfade");
        assert!(!isolate_boundary_crossed(false, false, false), "a non-isolate edit must never crossfade");
    }

    /// End-to-end through the actual `SlotAssignment` a drag produces: a bandpass sweeping from
    /// 9000 Hz to 31 Hz in one throttled step (more than an octave — `slot_reusable` correctly
    /// refuses to hand it the sweep's own slot) must be recognised as a boundary crossing, the
    /// same live scenario reported (the click path was fixed; dragging into the low range while
    /// still holding the button reproduced the same artefact because this case was missed).
    #[test]
    fn a_far_drag_jump_through_real_slot_assignment_is_recognised_as_a_boundary() {
        let mut sa = SlotAssignment::default();
        let entry_bandpass = Band { kind: FilterKind::Bandpass, freq_hz: 9000.0, gain_db: 0.0, q: 8.0 };
        let was_isolate = false;
        let is_isolate = is_isolate_audition(std::slice::from_ref(&entry_bandpass));
        let prev_isolate_slot = sa.isolate_slot;
        sa.assign(std::slice::from_ref(&entry_bandpass));
        sa.was_isolate = is_isolate;
        assert!(
            isolate_boundary_crossed(was_isolate, is_isolate, sa.isolate_slot != prev_isolate_slot),
            "entering must crossfade"
        );

        // The drag continues, jumping straight down to 31 Hz — well past `MAX_REUSE_FC_RATIO`.
        let drag_bandpass = Band { kind: FilterKind::Bandpass, freq_hz: 31.0, gain_db: 0.0, q: 8.0 };
        let was_isolate = sa.was_isolate;
        let is_isolate = is_isolate_audition(std::slice::from_ref(&drag_bandpass));
        let prev_isolate_slot = sa.isolate_slot;
        sa.assign(std::slice::from_ref(&drag_bandpass));
        let isolate_slot_changed = sa.isolate_slot != prev_isolate_slot;
        assert!(isolate_slot_changed, "9000 Hz -> 31 Hz must not reuse the same slot");
        assert!(
            isolate_boundary_crossed(was_isolate, is_isolate, isolate_slot_changed),
            "a far jump mid-drag must still be recognised as a boundary crossing"
        );
    }

    /// **The bug this fixes, reported live**: dragging at "medium" speed, in either direction,
    /// could still reproduce the same audible/visible sweep-and-lurch artefact even after the
    /// crossfade fix above shipped — rarer, but the same pattern. Root cause: a drag that jumps
    /// far enough to open a fresh slot (as above) leaves the *old* slot behind as an abandoned,
    /// independently-fading gap — and if the drag later swings back near that old position, the
    /// generic identity search in `SlotAssignment::assign` used to reclaim the OLD, abandoned
    /// slot (still `slot_reusable`-compatible by Fc/Q, and sitting at a lower index) instead of
    /// retargeting the band's actual current slot. The array can *shrink* when that happens (the
    /// true current slot becomes the new trailing gap and gets trimmed), which the old
    /// length-only check (`out.len() > slots_before`) read as "nothing new happened" — exactly
    /// backwards. `assign_isolate` fixes this by tracking the live slot explicitly instead of
    /// searching for any match.
    #[test]
    fn a_drag_that_swings_back_near_an_abandoned_slot_still_tracks_the_current_one() {
        let mut sa = SlotAssignment::default();
        let far = Band { kind: FilterKind::Bandpass, freq_hz: 9000.0, gain_db: 0.0, q: 8.0 };
        let low = Band { kind: FilterKind::Bandpass, freq_hz: 31.0, gain_db: 0.0, q: 8.0 };
        // Close to `far`'s old position (well within an octave), far from `low`'s.
        let back = Band { kind: FilterKind::Bandpass, freq_hz: 8000.0, gain_db: 0.0, q: 8.0 };

        let first = sa.assign(std::slice::from_ref(&far));
        assert_eq!(first.len(), 1);
        let far_slot = sa.isolate_slot;

        let second = sa.assign(std::slice::from_ref(&low));
        assert_eq!(second.len(), 2, "far apart, must open a new slot rather than reuse `far`'s");
        assert!(second[far_slot.unwrap()].is_none(), "the old (far) slot must be an explicit gap now");
        let low_slot = sa.isolate_slot;
        assert_ne!(low_slot, far_slot, "the band's current slot must be the new one, not the old one");

        // Swing back near the FIRST position — close to the old, abandoned `far` slot (8000 Hz
        // vs. its remembered 9000 Hz — well inside `MAX_REUSE_FC_RATIO`), nowhere near where the
        // band actually, currently is (`low`'s slot, at 31 Hz). Since `back` isn't close to the
        // *tracked* slot either, this is a genuine new jump — it must land in a fresh slot of
        // its own, and critically must NOT resurrect the old `far` slot just because it happens
        // to match by Fc/Q: that slot has been independently fading since round 2, and reusing
        // it is exactly the bug this test guards.
        let third = sa.assign(std::slice::from_ref(&back));
        assert!(third[far_slot.unwrap()].is_none(), "the old (far) slot must stay untouched, not get reclaimed");
        assert!(third[low_slot.unwrap()].is_none(), "the old (low) slot must also stay an explicit gap");
        let back_slot = sa.isolate_slot.expect("back must have landed somewhere");
        assert_ne!(back_slot, far_slot.unwrap(), "must NOT resurrect the abandoned far slot");
        assert_ne!(back_slot, low_slot.unwrap(), "back is not close to low either — must be a fresh slot");
        assert_eq!(third[back_slot].map(|b| b.freq_hz), Some(8000.0), "back must land in its own, genuinely new slot");

        // And the actual decision `push_live` makes off this: landing on a different slot than
        // the previous push must still be recognised as a boundary crossing, exactly like the
        // ordinary far-jump case — this is what makes the fix reach the crossfade path at all,
        // not just get the bookkeeping right in isolation.
        assert!(
            isolate_boundary_crossed(true, true, back_slot != low_slot.unwrap()),
            "landing on a different slot than last time must still trigger the crossfade"
        );
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
    /// forever — as long as it's close enough in `Fc` to trust a direct ramp between the two
    /// (see `fc_close_enough`). This one lands well inside `MAX_REUSE_FC_RATIO` of the freed
    /// slot's old centre.
    #[test]
    fn a_new_band_reuses_a_freed_slot_before_growing() {
        let mut sa = SlotAssignment::default();
        let low = band(100.0, 3.0, 0.7);
        let mid = band(1000.0, -2.0, 1.0);
        sa.assign(&[low, mid]);
        sa.assign(&[low]); // drop mid, freeing slot 1

        let near_mid = band(1400.0, 1.0, 1.0);
        let low_near = [low, near_mid];
        let third = sa.assign(&low_near);
        assert_eq!(third.len(), 2, "the new band reused the freed slot instead of appending");
        assert_eq!(third[1].unwrap().freq_hz, 1400.0);
    }

    /// The case the reuse limit exists for: a freed slot must NOT be handed to a new band whose
    /// `Fc` is nowhere near what used to live there — that would make `Cascade::start_ramp`
    /// sweep straight from 1 kHz to 15 kHz instead of two independent fades. The old slot keeps
    /// fading in place (a genuine mid-array gap, per `removing_a_middle_band_...` above — a
    /// two-band drop-the-last-one would just shrink, so `high` has to stay present after `mid`
    /// is dropped to keep this a mid-array gap) and the new band gets a slot of its own.
    #[test]
    fn a_distant_band_does_not_reuse_a_freed_slot() {
        let mut sa = SlotAssignment::default();
        let low = band(100.0, 3.0, 0.7);
        let mid = band(1000.0, -2.0, 1.0);
        let high = band(8000.0, 4.0, 0.7);
        sa.assign(&[low, mid, high]);

        let low_high = [low, high];
        sa.assign(&low_high); // drop mid — slot 1 fades in place, high stays in slot 2

        let distant = band(15000.0, 1.0, 1.0);
        let low_high_distant = [low, high, distant];
        let third = sa.assign(&low_high_distant);
        assert_eq!(third.len(), 4, "the distant band must not reuse mid's slot — it needs its own");
        assert_eq!(third[0].unwrap().freq_hz, 100.0, "low keeps its slot");
        assert!(third[1].is_none(), "mid's old slot keeps fading, untouched by the unrelated band");
        assert_eq!(third[2].unwrap().freq_hz, 8000.0, "high keeps its slot");
        assert_eq!(third[3].unwrap().freq_hz, 15000.0, "the distant band gets a fresh slot instead");

        // And a later band that genuinely is close to what slot 1 used to hold can still
        // reclaim it — the reservation isn't permanent, just distance-gated.
        let reclaim = band(900.0, 1.0, 1.0);
        let low_reclaim_high_distant = [low, reclaim, high, distant];
        let fourth = sa.assign(&low_reclaim_high_distant);
        assert_eq!(fourth.len(), 4, "no new slot needed — the close band reclaimed the old one");
        assert_eq!(fourth[1].unwrap().freq_hz, 900.0, "close enough to mid's old 1 kHz to reuse slot 1");
    }

    /// The bug report this test guards against: the §5.2 isolate sweep's bandpass (always
    /// `Bandpass`, `Q` fixed at `SWEEP_Q`) passing within an octave of an existing shelf's `Fc`
    /// must NOT reuse that shelf's freed slot — a shelf and a bandpass don't share a response
    /// shape, so `Cascade::start_ramp` interpolating one's raw coefficients into the other's is
    /// audible as a lurch through an unrelated filter, not a "close enough" nudge. The kind gate
    /// must refuse this even though Fc (and here, coincidentally, nothing about Q) would pass.
    #[test]
    fn a_different_filter_kind_does_not_reuse_a_freed_slot_even_at_the_same_fc() {
        let mut sa = SlotAssignment::default();
        let low = band(100.0, 3.0, 0.7);
        let shelf = Band { kind: FilterKind::LowShelf, freq_hz: 120.0, gain_db: 4.0, q: 0.7 };
        let high = band(8000.0, 4.0, 0.7);
        sa.assign(&[low, shelf, high]);
        let low_high = [low, high];
        sa.assign(&low_high); // drop the shelf (a middle band) — its slot fades in place, high stays put

        let bandpass = Band { kind: FilterKind::Bandpass, freq_hz: 120.0, gain_db: 0.0, q: 8.0 };
        let low_bandpass_high = [low, bandpass, high];
        let third = sa.assign(&low_bandpass_high);
        assert_eq!(third.len(), 4, "the bandpass must get its own slot, not the shelf's freed one");
        assert!(third[1].is_none(), "the shelf's old slot keeps fading, untouched by a different kind");
        assert_eq!(third[3].unwrap().kind, FilterKind::Bandpass, "the bandpass lands in a fresh slot");
    }

    /// Same kind and close `Fc`, but a bandwidth so different it isn't "the same band" either —
    /// a broad, gentle band and a sharp, narrow one at nearly the same centre are two different
    /// shapes, and interpolating between them sweeps through everything in between just like an
    /// unchecked `Fc` jump would.
    #[test]
    fn a_wildly_different_q_does_not_reuse_a_freed_slot_even_at_the_same_fc() {
        let mut sa = SlotAssignment::default();
        let low = band(100.0, 3.0, 0.7);
        let broad = band(1000.0, -2.0, 0.5);
        let high = band(8000.0, 4.0, 0.7);
        sa.assign(&[low, broad, high]);
        let low_high = [low, high];
        sa.assign(&low_high); // drop the broad band (a middle band) — its slot fades, high stays put

        let narrow = band(1000.0, 1.0, 6.0); // ratio 12, well past MAX_REUSE_Q_RATIO
        let low_narrow_high = [low, narrow, high];
        let third = sa.assign(&low_narrow_high);
        assert_eq!(third.len(), 4, "the narrow band must get its own slot, not the broad one's");
        assert!(third[1].is_none(), "the broad band's old slot keeps fading, untouched");
        assert_eq!(third[3].unwrap().q, 6.0, "the narrow band lands in a fresh slot");
    }

    /// The actual isolate scenario — a full multi-band correction (a low shelf plus peaking
    /// bands) replaced wholesale by a single isolate bandpass. Every old band must end up an
    /// explicit gap (fading to passthrough on its own slot), and the bandpass must land in a
    /// brand new slot, never reusing any of them directly. Broader than
    /// `a_different_filter_kind_does_not_reuse_a_freed_slot_even_at_the_same_fc` (which isolates
    /// just the kind gate): this is the whole `push_live` input shape isolate actually produces.
    #[test]
    fn a_full_correction_yields_entirely_to_an_isolate_bandpass() {
        let mut sa = SlotAssignment::default();
        let shelf = Band { kind: FilterKind::LowShelf, freq_hz: 100.0, gain_db: 5.0, q: 0.7 };
        let mid = band(1000.0, -2.0, 1.0);
        let high = band(8000.0, 3.0, 0.7);
        let shelf_mid_high = [shelf, mid, high];
        sa.assign(&shelf_mid_high);

        // The bandpass's Fc (150 Hz) sits well within the shelf's old Fc's reuse ratio — this is
        // exactly the case the old Fc-only check would have handed the shelf's slot to.
        let bandpass = Band { kind: FilterKind::Bandpass, freq_hz: 150.0, gain_db: 0.0, q: 8.0 };
        let just_bandpass = [bandpass];
        let second = sa.assign(&just_bandpass);
        assert_eq!(second.len(), 4, "three old bands fade in place, the bandpass gets a 4th slot");
        for (i, b) in second[..3].iter().enumerate() {
            assert!(b.is_none(), "slot {i} must be an explicit gap, not reused by the bandpass");
        }
        assert_eq!(second[3].unwrap().kind, FilterKind::Bandpass, "the bandpass lands in a fresh slot");
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
