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

/// How long a config file's write waits for the endpoint's edits to go quiet before it
/// actually happens — see [`CageqApoBackend::queue_write`]'s own doc for why it waits at all.
/// Well past the app's own ~60-70 ms live-edit coalescing cadence (so ordinary dragging never
/// triggers a mid-drag write), short enough that "settled" reads as immediate to a person.
const WRITE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(500);

/// Applies filters through CAGEq's own APO. Holds the directory corrections live in;
/// everything else is derived from an endpoint id.
///
/// `slots`: per-endpoint live-push slot assignment (§5.3c "identity-matched ramps") — see
/// [`SlotAssignment`]'s own doc for what it's for. `Mutex`, not `RefCell`: [`EqBackend::apply`]
/// takes `&self`, so this is the interior mutability that needs, and the app already shares one
/// `CageqApoBackend` behind an `Arc` rather than cloning it — nothing actually needed `Clone`,
/// which a bare `Mutex` field can't derive anyway.
///
/// Endpoint id -> `(safe_epoch at queue time, rendered text)`. The epoch is what lets a flush
/// refuse a write that has been superseded by a safe-state trip since it was queued — see
/// [`CageqApoBackend::queue_write`] and [`CageqApoBackend::write_safe_state`].
type PendingMap = std::collections::HashMap<String, (u64, String)>;

/// `pending`/`write_tx`: the debounced half of persistence — see [`CageqApoBackend::queue_write`].
#[derive(Debug)]
pub struct CageqApoBackend {
    config_dir: PathBuf,
    slots: std::sync::Mutex<std::collections::HashMap<String, SlotAssignment>>,
    /// Endpoint id -> the exact text that endpoint's file will have once the debounced writer
    /// catches up. Overlaid on top of whatever is already on disk wherever the two disagree —
    /// see [`CageqApoBackend::effective_endpoint_ids`] and [`CageqApoBackend::applied_hash`],
    /// both of which need "what was just applied", not "what has physically hit the disk yet".
    pending: std::sync::Arc<std::sync::Mutex<PendingMap>>,
    /// Bumped by [`EqBackend::write_safe_state`], before it does anything else. Every
    /// `queue_write` stamps its entry with whatever this reads *at the time it is queued*, and
    /// a flush refuses to write any entry stamped with an older epoch than this reads *at flush
    /// time* — closing a real race: a live edit's queued write landing debounced, *after* a
    /// watchdog trip, would otherwise silently overwrite the safe state that trip just wrote.
    /// Shared with the background writer via the same `Arc` `pending` already needs.
    safe_epoch: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Wakes the background writer thread on every [`CageqApoBackend::queue_write`] — it
    /// doesn't matter *what* is sent, only that something was; see that thread's own doc for
    /// how the debounce itself works. Dropping this (with `CageqApoBackend` itself) is what
    /// tells the thread to flush one last time and exit.
    write_tx: std::sync::mpsc::Sender<()>,
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
        let config_dir = config_dir.into();
        let pending = std::sync::Arc::new(std::sync::Mutex::new(PendingMap::new()));
        let safe_epoch = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (write_tx, write_rx) = std::sync::mpsc::channel::<()>();
        spawn_debounced_writer(
            config_dir.clone(),
            std::sync::Arc::clone(&pending),
            std::sync::Arc::clone(&safe_epoch),
            write_rx,
        );
        CageqApoBackend {
            config_dir,
            slots: std::sync::Mutex::new(std::collections::HashMap::new()),
            pending,
            safe_epoch,
            write_tx,
        }
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

    /// Every endpoint id that has a correction right now, on disk or still queued — see
    /// [`CageqApoBackend::queue_write`]. Neither source alone is enough: a brand-new endpoint
    /// whose very first write hasn't landed yet has no file at all. Used where "what was just
    /// applied" means specifically *persisted or about-to-be* content — [`Self::applied_hash`].
    fn effective_endpoint_ids(&self) -> Vec<String> {
        let mut ids: std::collections::HashSet<String> = self
            .config_files()
            .iter()
            .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(str::to_string))
            .collect();
        ids.extend(self.pending.lock().unwrap().keys().cloned());
        let mut ids: Vec<String> = ids.into_iter().collect();
        ids.sort();
        ids
    }

    /// Every endpoint [`EqBackend::write_safe_state`] must cover — broader than
    /// [`Self::effective_endpoint_ids`]: it also includes every endpoint `push_live` has *ever*
    /// live-pushed to this session (`self.slots`'s keys), even one whose very first correction
    /// was a §5.2 isolate audition and so was never written or queued at all (isolate is
    /// live-only by design — see `is_isolate_filters`'s own doc). Audio can be live on that
    /// endpoint, running the boosted narrow bandpass isolate pushes, with nothing in
    /// `effective_endpoint_ids` to show for it; a watchdog trip landing at that exact moment
    /// must not skip silencing it just because no *file-worthy* correction ever existed there.
    fn endpoints_needing_safe_state(&self) -> Vec<String> {
        let mut ids: std::collections::HashSet<String> = self.effective_endpoint_ids().into_iter().collect();
        ids.extend(self.slots.lock().unwrap().keys().cloned());
        let mut ids: Vec<String> = ids.into_iter().collect();
        ids.sort();
        ids
    }

    /// Hash of everything currently applied, across every endpoint.
    ///
    /// One hash for many files, because settings.json remembers exactly one per §3.0. Built
    /// from filename *and* content so that adding, removing or renaming a device's correction
    /// all register as a change — content alone would miss a file being deleted when another
    /// identical one exists.
    ///
    /// Reads a queued-but-not-yet-flushed endpoint's *pending* text rather than its (older, or
    /// absent) file — this is what makes [`EqBackend::apply`]'s returned hash describe what was
    /// just applied rather than what has physically hit disk yet. At startup, before anything
    /// in this session has queued a write, `pending` is empty and this reduces to reading disk
    /// exactly as it always did — which is exactly what [`EqBackend::startup_decision`] needs.
    fn applied_hash(&self) -> String {
        // The endpoint list first, on its own: `effective_endpoint_ids` takes `pending`'s lock
        // itself, and `self.pending`'s `Mutex` is not re-entrant — holding it across that call
        // would deadlock this thread against itself.
        let ids = self.effective_endpoint_ids();
        let pending = self.pending.lock().unwrap();
        // A pending entry stamped older than the *current* safe-state epoch is stale — it was
        // superseded by a safe-state trip since it was queued (see `safe_epoch`'s own doc) and
        // will never actually be written (`flush_batch` drops it on the same check). Trusting
        // it here would report the hash of a correction that is not, and will not become, what
        // is on disk — falling back to disk instead is what actually landed, or will.
        let current_epoch = self.safe_epoch.load(std::sync::atomic::Ordering::SeqCst);
        let mut hasher = Sha256::new();
        for id in ids {
            hasher.update(format!("{id}.cfg").as_bytes());
            let body = match pending.get(&id) {
                Some((epoch, text)) if *epoch >= current_epoch => Some(text.clone()),
                _ => fs::read_to_string(config::config_path_in(&self.config_dir, &id)).ok(),
            };
            if let Some(body) = body {
                hasher.update(strip_hash_line(&body).as_bytes());
            }
        }
        short_hex(&hasher.finalize())
    }

    /// Write one endpoint's correction, and push it live if the APO is running there.
    /// Write one endpoint's correction, and push it live if the APO is running there.
    ///
    /// **Except for the §5.2 isolate audition, which is live-only — never written to disk, not
    /// even queued.** It is transient monitoring state, not "the correction": persisting it
    /// would leave the *audition* as what §3.0's startup check resumes into next launch if
    /// CAGEq crashed mid-drag, not the real correction underneath it.
    fn apply_one(&self, cfg: &DeviceConfig) -> Result<(), ApoBackendError> {
        let id = config::normalize_endpoint_id(&cfg.device)
            .ok_or_else(|| ApoBackendError::BadEndpointId(cfg.device.clone()))?;
        let apo_config = to_apo_config(cfg);

        if !is_isolate_filters(&cfg.filters) {
            self.queue_write(&id, &apo_config);
        }
        // No channel simply means nothing is playing on that endpoint — the queued write is
        // the whole job in that case, and the APO will read the file it eventually becomes at
        // its next `LockForProcess`; for isolate specifically, nothing to audition against
        // means nothing to do at all. A genuine refusal is different: the live engine has
        // explicitly declined to run this correction (see `LivePushRefused`'s own doc for why
        // that's worth surfacing rather than swallowing — this used to be indistinguishable
        // from the benign "no channel" case, both discarded the same way, silently).
        match self.push_live(&id, &apo_config)? {
            PushOutcome::NoChannel | PushOutcome::Published => Ok(()),
            PushOutcome::Refused => Err(ApoBackendError::LivePushRefused(id)),
        }
    }

    /// Render `apo_config` exactly as [`CageqApoBackend::write_config`] would, hash line and
    /// all — shared so the debounced and the direct-write paths can never drift into producing
    /// different bytes for the same input.
    fn render_with_hash(apo_config: &ApoConfig) -> String {
        let body = config::render(apo_config);
        format!("{HASH_PREFIX}{}\n{body}", short_hex(&Sha256::digest(body.as_bytes())))
    }

    /// Queue `apo_config` to be written for `id` once the endpoint's edits go quiet
    /// ([`WRITE_DEBOUNCE`]), instead of writing it — `fs::write` + `fs::rename` — on this call.
    ///
    /// **Why defer at all.** The config file exists purely for restart persistence (see this
    /// module's own doc) — nothing about *live* editing needs it; that is the control channel's
    /// job, and it is already fast (an in-memory push, no disk touched). Before this, every
    /// single throttled push — including every isolate-style live drag tick — paid a
    /// synchronous file write before the frame it was answering could even finish, serialized
    /// behind the app's own one-write-at-a-time gate: on a fast or erratic drag that backlog
    /// visibly and audibly compounded into multi-second lag. Batching every rapid edit down to
    /// one write, timed to when the user has actually stopped, removes that disk round trip
    /// from the live-edit path entirely — the ordinary case (someone actively tuning a filter)
    /// now touches disk once they pause, not once per frame.
    ///
    /// The write itself happens on a dedicated background thread (spawned in [`Self::new`]),
    /// not on whatever thread calls this — so this call is a map insert and a channel send,
    /// both effectively free, and returns immediately regardless of the debounce window or
    /// however slow the eventual disk write turns out to be.
    ///
    /// **What this does not weaken.** `applied_hash`/`effective_endpoint_ids` read `pending`
    /// directly, so nothing that depends on "what was just applied" (§3.0's resume hash, the
    /// safe-state writer covering every live endpoint) can observe a gap just because the
    /// physical write hasn't happened yet — see both their own docs. The one real trade-off is
    /// an abrupt process kill inside the debounce window losing the last unsettled edit; a
    /// clean exit does not have to (see [`CageqApoBackend::flush_pending`]).
    fn queue_write(&self, id: &str, apo_config: &ApoConfig) {
        let text = Self::render_with_hash(apo_config);
        // Stamped with the epoch *now*, so a flush that happens after a later safe-state trip
        // can recognise this entry as pre-dating it — see `safe_epoch`'s own doc.
        let epoch = self.safe_epoch.load(std::sync::atomic::Ordering::SeqCst);
        self.pending.lock().unwrap().insert(id.to_string(), (epoch, text));
        // The receiver can only be gone if `self` itself is mid-drop; either way there is
        // nothing to do about a failed send here, and `Drop`'s own final flush (via the
        // channel disconnecting) covers the pending entry regardless.
        let _ = self.write_tx.send(());
    }

    /// Write out everything currently queued right now, synchronously, without waiting for
    /// [`WRITE_DEBOUNCE`] to elapse on its own. Two callers: tests (which need deterministic
    /// timing rather than a real sleep) and a clean app shutdown, so the very last edit before
    /// closing isn't left to a debounce window the process may not stay alive to finish.
    ///
    /// Reports the first failure encountered (matching the old, synchronous `write_config`'s
    /// contract as closely as a batch operation can), but does not stop at it — every other
    /// endpoint in the batch still gets its own attempt, and whichever ones fail (for any
    /// reason, including having been superseded by a safe-state trip since they were queued —
    /// see [`Self::safe_epoch`]) are put back into `pending` rather than silently dropped, so
    /// the next flush (debounced or explicit) retries them instead of losing the edit outright.
    pub fn flush_pending(&self) -> Result<(), ApoBackendError> {
        let epoch = self.safe_epoch.load(std::sync::atomic::Ordering::SeqCst);
        let (failed, first_err) = flush_batch(&self.config_dir, epoch, drain(&self.pending));
        requeue(&self.pending, failed);
        match first_err {
            None => Ok(()),
            Some(e) => Err(e),
        }
    }

    fn write_config(&self, id: &str, apo_config: &ApoConfig) -> Result<(), ApoBackendError> {
        let path = config::config_path_in(&self.config_dir, id);
        write_atomic(&path, &Self::render_with_hash(apo_config))
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

/// Flushes whatever is still queued when the backend goes away, so a clean app shutdown never
/// loses the last unsettled edit to [`WRITE_DEBOUNCE`]'s window — the background writer would
/// eventually catch it too (its own `Disconnected` arm does the same flush), but that thread
/// outliving process shutdown by even a few hundred ms is not something to rely on. Effectively
/// a no-op in the ordinary case and a safety net only if a `queue_write` call raced this one.
impl Drop for CageqApoBackend {
    fn drop(&mut self) {
        let _ = self.flush_pending();
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
        // Silences every endpoint CAGEq has a correction for — which is exactly the set it can
        // affect, so enumerating audio devices (which can fail) is not needed on a path that
        // has to be as close to infallible as possible. `endpoints_needing_safe_state`, not
        // `config_files`, so neither an endpoint whose very first correction is still sitting
        // in `pending` (never yet reached disk — see `queue_write`) nor one whose *only* live
        // push ever was a §5.2 isolate audition (deliberately never written or queued at all —
        // see `is_isolate_filters`'s own doc, and that method's own doc) is missed just because
        // neither has a file yet; a watchdog trip can arrive at any moment, including
        // immediately after a live edit or mid-isolate-drag.
        //
        // Bumped *before* touching anything else: every `queue_write` from here on stamps its
        // entry with the new epoch, and every entry already queued keeps the old one — which is
        // exactly the distinction the debounced flush needs (`flush_batch`) to refuse writing a
        // pre-trip edit over the safe state this call is about to write, however that edit's
        // queue and this trip happen to interleave in real time. `applied_hash` makes the same
        // check on the read side, so a stale entry lingering in `pending` afterwards (nothing
        // here removes it — the epoch alone is enough) cannot be mistaken for what is on disk
        // either.
        self.safe_epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let safe = safe_state();
        // Both halves matter: the file makes the safe state survive a restart, and the live
        // push makes it take effect *now*, which is the entire point of a watchdog fail-safe.
        // Waiting for the next `LockForProcess` would be no fail-safe at all. Written directly
        // (not queued) — a fail-safe has to land now, not after a debounce window.
        for id in &self.endpoints_needing_safe_state() {
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

/// Is `filters` the §5.2 isolate audition — a bandpass-only substitute for the real correction,
/// used to hear one filter's region in isolation? Always exactly one `Bandpass` band; anything
/// else (an ordinary correction, a shelf/peaking band alone, two bandpasses) is not. Works over
/// `cageq_backend::Filter`/`FilterType` rather than the APO's own `Band`/`FilterKind` because
/// `apply_one` sees `cfg.filters` *before* `to_apo_config` converts it — a second copy rather
/// than a shared one, the same tradeoff [`band_key`]'s own doc already makes for this exact
/// pair of types.
fn is_isolate_filters(filters: &[Filter]) -> bool {
    filters.len() == 1 && filters[0].kind == FilterType::Bandpass
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

/// Is `old` — a freed slot's last known identity, or `None` if the slot has never held one —
/// close enough to `band` for [`SlotAssignment::assign`] to reuse that slot? A slot that never
/// held anything is always fair game: there is no live interpolation to sweep away from.
fn fc_close_enough(old: &Option<(u8, i64, i64)>, band: &Band) -> bool {
    let Some((_, old_freq_milli_hz, _)) = old else { return true };
    let old_hz = *old_freq_milli_hz as f64 / 1000.0;
    if !(old_hz > 0.0 && band.freq_hz > 0.0) {
        return false;
    }
    let ratio = (band.freq_hz / old_hz).max(old_hz / band.freq_hz);
    ratio <= MAX_REUSE_FC_RATIO
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
///
/// A freed slot isn't fair game for just *any* new band, either — see [`fc_close_enough`]. So a
/// slot's remembered identity survives rounds where nothing reused it (it stays fading toward
/// passthrough, untouched), rather than being wiped back to "empty" the moment its own band
/// drops; only [`MAX_BANDS`](dsp::MAX_BANDS)-headroom pays for that, not correctness.
#[derive(Debug, Default)]
struct SlotAssignment {
    /// Index i is the identity last assigned to that slot — kept even once that band is gone
    /// and the slot is fading toward passthrough, so a later unrelated band can't silently
    /// inherit its in-flight ramp (see [`fc_close_enough`]). `None` only for a slot that has
    /// never held a band at all.
    slots: Vec<Option<(u8, i64, i64)>>,
}

impl SlotAssignment {
    /// Reorders `bands` into slot order: a band whose identity matches a previous slot stays in
    /// that exact slot. Everything else (genuinely new, or a duplicate identity beyond the first
    /// match — rare, and not worth more bookkeeping to handle perfectly) reuses the lowest slot
    /// that is both free *and* [`fc_close_enough`] to its last occupant, or opens a fresh one
    /// past the end if none qualifies. Trailing frees are trimmed (the existing count-shrink
    /// behaviour already covers those); a freed slot with something later still live stays as an
    /// explicit gap — and keeps remembering what it held, so it stays off-limits to a distant
    /// band for as long as it takes something close enough to come reclaim it.
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
            let reuse_at = out
                .iter()
                .zip(self.slots.iter())
                .position(|(occupant, old)| occupant.is_none() && fc_close_enough(old, band));
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

/// Take everything currently queued in one atomic step, leaving `pending` empty. Used by both
/// the background writer (on its debounce timeout) and [`CageqApoBackend::flush_pending`] — the
/// mutex means only one of them can ever actually drain a given entry, so the two can race each
/// other harmlessly (whichever gets there first does the work; the other finds nothing left)
/// rather than double-writing or losing one.
fn drain(pending: &std::sync::Mutex<PendingMap>) -> Vec<(String, (u64, String))> {
    pending.lock().unwrap().drain().collect()
}

/// Put entries back into `pending` for the next flush to retry — used after a batch comes back
/// with genuine I/O failures (see [`flush_batch`]). `or_insert`, not a blind overwrite: a fresh
/// `queue_write` for the same id may have landed *while* this batch was in flight, and that
/// newer content must win over the stale content that just failed to write, not be clobbered by
/// it.
fn requeue(pending: &std::sync::Mutex<PendingMap>, failed: Vec<(String, (u64, String))>) {
    if failed.is_empty() {
        return;
    }
    let mut p = pending.lock().unwrap();
    for (id, entry) in failed {
        p.entry(id).or_insert(entry);
    }
}

/// Write every `(endpoint id, (epoch, rendered text))` pair to its config file — except an
/// entry stamped with an epoch older than `current_epoch`, which is dropped **silently, not as
/// a failure**: it was legitimately superseded by a safe-state trip since it was queued (see
/// [`CageqApoBackend::safe_epoch`]'s own doc), and writing it now would undo the safe state that
/// trip just wrote.
///
/// Continues past a genuine write failure rather than stopping at the first one — a transient
/// failure for one endpoint (an antivirus scan holding the file, say) must not also block every
/// other endpoint's write. Failed entries are returned for the caller to [`requeue`] rather than
/// losing them outright; the first failure's error is returned too, so at least one still
/// surfaces somewhere instead of vanishing into a background thread with nothing watching it.
fn flush_batch(
    config_dir: &Path,
    current_epoch: u64,
    batch: Vec<(String, (u64, String))>,
) -> (Vec<(String, (u64, String))>, Option<ApoBackendError>) {
    let mut failed = Vec::new();
    let mut first_err = None;
    for (id, (epoch, text)) in batch {
        if epoch < current_epoch {
            continue;
        }
        if let Err(e) = write_atomic(&config::config_path_in(config_dir, &id), &text) {
            eprintln!("[cageq-apo-backend] failed to write config for {id}: {e}");
            if first_err.is_none() {
                first_err = Some(e);
            }
            failed.push((id, (epoch, text)));
        }
    }
    (failed, first_err)
}

/// The debounced writer itself: sleeps between pokes from [`CageqApoBackend::queue_write`], and
/// only actually touches disk once [`WRITE_DEBOUNCE`] has passed with no new poke — see that
/// method's own doc for why deferring at all is worth doing. One thread per `CageqApoBackend`,
/// spawned in [`CageqApoBackend::new`]; torn down (after one final flush) when `write_tx` is
/// dropped alongside the backend.
fn spawn_debounced_writer(
    config_dir: PathBuf,
    pending: std::sync::Arc<std::sync::Mutex<PendingMap>>,
    safe_epoch: std::sync::Arc<std::sync::atomic::AtomicU64>,
    rx: std::sync::mpsc::Receiver<()>,
) {
    std::thread::spawn(move || loop {
        match rx.recv_timeout(WRITE_DEBOUNCE) {
            // Fresh activity. Nothing to do but loop back into `recv_timeout` — restarting the
            // debounce window is *implicit* in calling it again from here rather than from a
            // remembered deadline.
            Ok(()) => continue,
            // Quiet for the full window: whatever is queued has settled — write it.
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let epoch = safe_epoch.load(std::sync::atomic::Ordering::SeqCst);
                let (failed, _) = flush_batch(&config_dir, epoch, drain(&pending));
                requeue(&pending, failed);
            }
            // The backend was dropped. Its `Drop` impl already flushed synchronously before
            // `write_tx` went away, so this is very likely a no-op — but if a `queue_write`
            // call raced the drop, this is the last chance to catch what it queued.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // Nothing will ever read `pending` again after this, so a failure here (unlike
                // the timeout arm above) has no next attempt to be requeued for — this really
                // is the last chance.
                let epoch = safe_epoch.load(std::sync::atomic::Ordering::SeqCst);
                let _ = flush_batch(&config_dir, epoch, drain(&pending));
                return;
            }
        }
    });
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
    const EP_LIVE_ISOLATE_ONLY: &str = "{99990003-0003-0003-0003-000000000003}";

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
        backend.flush_pending().unwrap(); // the write is debounced; force it to land now

        let path = config::config_path_in(&dir, EP);
        let text = fs::read_to_string(&path).expect("config file should exist");
        let parsed = config::parse(&text).expect("the APO's own parser must accept it");

        assert_eq!(parsed.preamp_db, -9.5);
        assert_eq!(parsed.bands.len(), 2);
        assert_eq!(parsed.bands[0].freq_hz, 105.0);
        assert_eq!(parsed.bands[0].gain_db, 6.0);
        assert_eq!(parsed.bands[1].kind, FilterKind::HighShelf);
    }

    /// The generalised version of the isolate fix: it was never really about isolate
    /// specifically — *any* rapid sequence of live edits used to pay a synchronous file write
    /// per push. An ordinary (non-isolate) `apply` must not touch disk immediately either; only
    /// once it settles (`flush_pending`, standing in for the real debounce window landing on
    /// its own) should the file actually appear.
    #[test]
    fn an_ordinary_apply_does_not_write_the_file_before_it_settles() {
        let (backend, dir) = temp_backend("ordinary-debounced");
        backend.apply(&[device(EP, -6.0)]).unwrap();

        assert!(
            !config::config_path_in(&dir, EP).exists(),
            "the write must not have landed yet — it should still be debounced"
        );

        backend.flush_pending().unwrap();
        assert!(
            config::config_path_in(&dir, EP).exists(),
            "once settled (flushed), the write must actually land"
        );
    }

    /// The safety-critical half of the same change: a watchdog trip can land at any moment,
    /// including the instant after a live edit whose write hasn't reached disk yet — even, for
    /// a brand-new endpoint, before it has ever had a file at all. `write_safe_state` must still
    /// cover it, not just whatever `config_files` (disk only) happens to already show.
    #[test]
    fn write_safe_state_covers_an_endpoint_whose_write_is_still_only_pending() {
        let (backend, dir) = temp_backend("safe-covers-pending");
        backend.apply(&[device(EP, -6.0)]).unwrap();
        assert!(
            !config::config_path_in(&dir, EP).exists(),
            "sanity check: the write really hasn't landed yet"
        );

        backend.write_safe_state().unwrap();

        let text = fs::read_to_string(config::config_path_in(&dir, EP))
            .expect("write_safe_state must cover a pending-only endpoint, not just files on disk");
        let parsed = config::parse(&text).expect("safe state must still parse");
        assert!(parsed.bands.is_empty(), "safe state should carry no filters");
        assert_eq!(parsed.preamp_db, -120.0, "safe state must silence, not neutralise");
    }

    /// The bug this guards: an endpoint whose very *first* live push was a §5.2 isolate
    /// audition never enters `pending` at all (isolate is deliberately live-only — see
    /// `is_isolate_filters`'s own doc) and has no file either, so `effective_endpoint_ids`
    /// alone would never see it — even though audio is genuinely running the boosted narrow
    /// bandpass on that endpoint right now (hence a real live channel, like the other
    /// live-channel tests in this file — `self.slots` is only ever populated once `push_live`
    /// gets far enough to reach it, which needs a channel to actually be open). A watchdog trip
    /// landing at that moment must still silence it.
    #[test]
    fn write_safe_state_covers_an_endpoint_whose_only_push_was_isolate() {
        let (backend, dir) = temp_backend("safe-covers-isolate-only");
        let Some(channel) = ControlChannel::create(EP_LIVE_ISOLATE_ONLY) else {
            eprintln!("skipping: could not create a Global\\ section (needs SeCreateGlobalPrivilege)");
            return;
        };
        cageq_apo::control::set_sample_rate(channel.block(), 48_000);

        let isolate = DeviceConfig {
            device: EP_LIVE_ISOLATE_ONLY.to_string(),
            preamp_db: 0.0,
            filters: vec![Filter { kind: FilterType::Bandpass, freq_hz: 31.0, gain_db: 0.0, q: 8.0 }],
        };
        backend.apply(&[isolate]).unwrap();
        assert!(
            !config::config_path_in(&dir, EP_LIVE_ISOLATE_ONLY).exists(),
            "sanity check: isolate must not have written or queued anything"
        );

        backend.write_safe_state().unwrap();

        let text = fs::read_to_string(config::config_path_in(&dir, EP_LIVE_ISOLATE_ONLY)).expect(
            "write_safe_state must cover an endpoint whose only push was isolate, not just \
             endpoints that already had a file-worthy correction",
        );
        let parsed = config::parse(&text).expect("safe state must still parse");
        assert!(parsed.bands.is_empty(), "safe state should carry no filters");
        assert_eq!(parsed.preamp_db, -120.0, "safe state must silence, not neutralise");
    }

    /// **The bug this guards**: a live edit racing a watchdog trip must never let its (now
    /// stale, pre-failure) content overwrite the safe state a moment later, once the debounced
    /// writer finally gets to it. `write_safe_state` deliberately does not clear `pending` — the
    /// epoch stamp alone (`safe_epoch`) must be enough to keep a stale entry from ever landing.
    #[test]
    fn write_safe_state_wins_over_a_stale_pending_write_even_if_one_lingers() {
        let (backend, dir) = temp_backend("safe-beats-stale-pending");
        // Queued but not yet flushed — this is the race: a live edit already in flight when the
        // watchdog fires.
        backend.apply(&[device(EP, -6.0)]).unwrap();

        backend.write_safe_state().unwrap();
        // The stale entry is still sitting in `pending` — nothing removed it. If the epoch
        // guard did not exist, this flush would overwrite the safe state just written above.
        assert!(backend.pending.lock().unwrap().contains_key(EP), "sanity check: the stale entry must still be queued");
        backend.flush_pending().unwrap();

        let text = fs::read_to_string(config::config_path_in(&dir, EP)).unwrap();
        let parsed = config::parse(&text).expect("must still parse");
        assert!(parsed.bands.is_empty(), "the stale pending write must not have overwritten the safe state");
        assert_eq!(parsed.preamp_db, -120.0, "the safe state must survive the stale write");
    }

    /// The pure mechanism behind the test above, isolated: `flush_batch` must silently skip
    /// (not write, not report as a failure) any entry stamped with an epoch older than the
    /// current one, and must still write everything stamped at or after it.
    #[test]
    fn flush_batch_skips_entries_older_than_the_current_epoch() {
        let dir = std::env::temp_dir()
            .join(format!("cageq-apo-backend-{}-flush-batch-epoch", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let batch = vec![
            ("stale-id".to_string(), (0u64, "stale content".to_string())),
            ("fresh-id".to_string(), (5u64, "fresh content".to_string())),
        ];
        let (failed, err) = flush_batch(&dir, 5, batch);
        assert!(failed.is_empty(), "neither entry should count as a *failure*");
        assert!(err.is_none());
        assert!(!dir.join("stale-id.cfg").exists(), "an epoch-stale entry must not be written");
        assert!(dir.join("fresh-id.cfg").exists(), "a current-epoch entry must still be written");
    }

    /// **The bug this guards**: before this, a write that failed (a transient disk/AV issue,
    /// say) was silently dropped — `queue_write` cannot fail at all, and the old
    /// `write_config`'s `Result` no longer reaches anyone once the write is debounced. A failed
    /// entry must be put back into `pending` so the next flush retries it, not lost outright.
    #[test]
    fn a_failed_write_is_requeued_not_lost() {
        let (backend, dir) = temp_backend("failed-write-requeue");
        backend.apply(&[device(EP, -6.0)]).unwrap();

        // Sabotage the write: put a directory exactly where `write_atomic`'s own temp file
        // needs to go, so `fs::write` fails predictably.
        let tmp_path = config::config_path_in(&dir, EP).with_extension("cfg.tmp");
        fs::create_dir_all(&tmp_path).unwrap();

        assert!(backend.flush_pending().is_err(), "the write must fail while the tmp path is a directory");
        assert!(
            backend.pending.lock().unwrap().contains_key(EP),
            "a failed write must be requeued, not lost"
        );

        // Clear the obstruction and retry: the requeued entry must actually land.
        fs::remove_dir(&tmp_path).unwrap();
        backend.flush_pending().unwrap();
        assert!(
            config::config_path_in(&dir, EP).exists(),
            "the requeued write must succeed once retried"
        );
    }

    /// The bug this guards: every isolate drag tick used to pay a synchronous file write
    /// before the live push even started, serialized behind the app's own one-write-at-a-time
    /// gate — compounding into multi-second lag on a fast drag. The §5.2 isolate audition
    /// (always exactly one `Bandpass`, see `FilterKind::Bandpass`'s own doc) must never touch
    /// the config file at all, live channel or not.
    #[test]
    fn the_isolate_audition_never_writes_the_config_file() {
        let (backend, dir) = temp_backend("isolate-no-write");
        let isolate = DeviceConfig {
            device: EP.to_string(),
            preamp_db: 0.0,
            filters: vec![Filter { kind: FilterType::Bandpass, freq_hz: 31.0, gain_db: 0.0, q: 8.0 }],
        };
        backend.apply(&[isolate]).unwrap();

        assert!(
            !config::config_path_in(&dir, EP).exists(),
            "an isolate-only apply must never create the config file"
        );
    }

    /// The other half of the same guard: an isolate apply must not silently *stop* writing an
    /// endpoint's real, already-persisted correction — only the isolate push itself is
    /// live-only, an ordinary apply right after it must still behave exactly as before.
    #[test]
    fn an_ordinary_apply_after_an_isolate_one_still_writes_the_file() {
        let (backend, dir) = temp_backend("isolate-then-ordinary");
        let isolate = DeviceConfig {
            device: EP.to_string(),
            preamp_db: 0.0,
            filters: vec![Filter { kind: FilterType::Bandpass, freq_hz: 31.0, gain_db: 0.0, q: 8.0 }],
        };
        backend.apply(&[isolate]).unwrap();
        backend.apply(&[device(EP, -6.0)]).unwrap();
        backend.flush_pending().unwrap(); // the write is debounced; force it to land now

        let text = fs::read_to_string(config::config_path_in(&dir, EP))
            .expect("the real correction must be written once isolate ends");
        let parsed = config::parse(&text).expect("the APO's own parser must accept it");
        assert_eq!(parsed.bands.len(), 2, "the real two-band correction, not the isolate bandpass");
    }

    /// The device id is normalised, so a bare GUID and a braced one address one endpoint
    /// rather than quietly producing two corrections that fight each other.
    #[test]
    fn endpoint_ids_are_normalised_and_bad_ones_refused() {
        let (backend, dir) = temp_backend("ids");
        backend.apply(&[device("6CAFE423-CDE5-4EC1-A1E2-E3FCEC778349", -3.0)]).unwrap();
        backend.flush_pending().unwrap(); // the write is debounced; force it to land now
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
        backend.flush_pending().unwrap(); // the write is debounced; force it to land now
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
        backend.flush_pending().unwrap(); // must be off disk, and out of `pending`, to remove

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
        backend.flush_pending().unwrap(); // the write is debounced; force it to land now
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
