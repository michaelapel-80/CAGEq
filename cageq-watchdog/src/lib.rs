//! Fail-safe watchdog / sidecar supervisor with recovery (filter.md §7.1 + §7.2).
//!
//! "Better no sound than wrong sound": the moment the DSP sidecar misbehaves, we
//! drive Equalizer APO into a hardcoded silent safe state (`Device: all` +
//! `Preamp: -120.0 dB`) written *by Rust*, independently of the (possibly dead)
//! Python process — then try to bring the sidecar back.
//!
//! ## Threads
//! The failure we must survive is "the sidecar hung and the driver is blocked in a
//! read." Whatever enforces the timeout therefore cannot be the blocked thread. So:
//!   * a **driver** thread owns the current [`Sidecar`], serves one request at a
//!     time, heartbeats when idle, and — because it also owns the spawn closure —
//!     performs restarts;
//!   * a **monitor** thread owns only a clock and the shared state; it trips on a
//!     blown deadline and *kills* the current sidecar to unblock the driver;
//!   * a short-lived **exit waiter** thread per sidecar generation blocks on the
//!     process handle ([`cageq_sidecar::Killer::wait`]) and trips the instant the
//!     process exits — event-based crash detection (§7.1) that doesn't wait for the
//!     next heartbeat. A generation counter plus the `Running` guard make a waiter
//!     for a replaced or intentionally-killed process a harmless no-op.
//!
//! They share an `Arc<Mutex<Shared>>`. The mutex is only ever held briefly and
//! never across a blocking call (a sidecar read, a spawn, a backoff sleep), so the
//! two threads never serialize on each other's slow work.
//!
//! ## Health state machine (§7.2)
//! ```text
//!   Running ──fault──► Recovering{1..N} ──ping ok──► Running
//!      ▲                     │
//!      │                     └──all N attempts fail──► Terminal ──retry()──► Recovering ─┐
//!      └───────────────────────────────────────────────────────────────────────────────┘
//! ```
//! Recovery is automatic only for *process* faults (crash/hang) — "the messenger
//! died". Anything pointing at a content/config problem (here: a failed safe-state
//! write) is non-recoverable and goes straight to `Terminal`, matching §7.2's rule
//! that blind auto-retry would just reproduce a content error. Restarts use the
//! §7.2 backoff schedule (default 2/5/10 s ⇒ 3 attempts); after they're exhausted,
//! `Terminal` waits for a single manual [`Supervisor::retry`].
//!
//! Cross-thread kill (the thing that makes hung-sidecar recovery possible) comes
//! from [`cageq_sidecar::Killer`], backed by `shared_child`.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cageq_config_writer::write_safe_state;
use cageq_sidecar::{Sidecar, SidecarError};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Configuration, health, outcomes
// ---------------------------------------------------------------------------

/// Deadlines, monitor granularity, and the restart backoff schedule. Defaults are
/// the §7.1/§7.2 values; tests shrink them so a run takes milliseconds.
#[derive(Debug, Clone)]
pub struct WatchdogConfig {
    /// Idle: send a heartbeat ping this often when nothing else is happening.
    pub idle_interval: Duration,
    /// Idle: a heartbeat ping must answer within this.
    pub idle_response: Duration,
    /// Busy: a real request must answer within this.
    pub busy_response: Duration,
    /// How often the monitor re-checks the deadline.
    pub tick: Duration,
    /// One entry per automatic restart attempt; the value is the pre-attempt wait.
    pub restart_backoffs: Vec<Duration>,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            idle_interval: Duration::from_secs(5),
            idle_response: Duration::from_secs(2),
            busy_response: Duration::from_secs(15),
            tick: Duration::from_millis(200),
            restart_backoffs: vec![
                Duration::from_secs(2),
                Duration::from_secs(5),
                Duration::from_secs(10),
            ],
        }
    }
}

/// Which deadline regime an operation was under when measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Idle,
    Busy,
}

/// Why the watchdog tripped to the safe state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TripReason {
    /// The sidecar process ended (EOF on stdout) — a crash or external kill.
    SidecarExited,
    /// A request or heartbeat blew its deadline without answering.
    Unresponsive { mode: Mode, waited: Duration },
    /// Writing the safe state itself failed — the gravest case: silence not even
    /// guaranteed. Non-recoverable (restarting the sidecar can't fix a disk error).
    SafeStateWriteFailed(String),
}

impl TripReason {
    /// Process faults recover automatically; a failed safe-state write does not.
    fn recoverable(&self) -> bool {
        matches!(self, TripReason::SidecarExited | TripReason::Unresponsive { .. })
    }
}

/// The supervisor's current health. Carries the trip reason while not `Running`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Health {
    /// Normal operation; the sidecar is serving requests.
    Running,
    /// Safe state is active; an automatic restart is in progress (`attempt` of N).
    Recovering { attempt: u32, reason: TripReason },
    /// Safe state is active; automatic restarts are exhausted (or the fault was
    /// non-recoverable). Awaits a manual [`Supervisor::retry`].
    Terminal { reason: TripReason },
}

/// Errors surfaced to a caller of [`Supervisor::call`].
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    /// Terminal safe state — won't self-heal; needs `retry()` or a fix.
    #[error("watchdog is in the terminal safe state: {0:?}")]
    Tripped(TripReason),
    /// Safe state active, restart in progress — try again shortly.
    #[error("watchdog is recovering the sidecar: {0:?}")]
    Recovering(TripReason),
    /// The driver thread is gone (shutting down). Shouldn't happen in normal use.
    #[error("sidecar driver is no longer running")]
    DriverGone,
    /// A transport/remote error for this specific request. A *remote* error (the
    /// sidecar answered but refused) passes through here and does NOT trip.
    #[error("sidecar error: {0}")]
    Sidecar(#[from] SidecarError),
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

struct InFlight {
    mode: Mode,
    started: Instant,
    deadline: Duration,
}

struct Shared {
    health: Health,
    in_flight: Option<InFlight>,
    /// Kill handle for the *current* sidecar; updated on every (re)spawn so the
    /// monitor always kills the right process.
    killer: Option<cageq_sidecar::Killer>,
    /// Bumped on every (re)spawn. An exit waiter captures the generation it was born
    /// under and only trips if it still matches — so a waiter for a replaced process
    /// can't cause a spurious trip.
    generation: u64,
    safe_state_path: PathBuf,
    shutdown: bool,
    manual_retry: bool,
    recoveries: u32,
}

impl Shared {
    /// Idempotent trip (both threads may call it): if currently `Running`, write the
    /// safe state and move to `Recovering` (recoverable fault) or `Terminal` (not).
    fn trip_if_running(&mut self, reason: TripReason) {
        if !matches!(self.health, Health::Running) {
            return;
        }
        // If we can't even write silence, that supersedes the original reason and is
        // non-recoverable.
        let reason = match write_safe_state(&self.safe_state_path) {
            Ok(()) => reason,
            Err(e) => TripReason::SafeStateWriteFailed(e.to_string()),
        };
        self.in_flight = None;
        self.health = if reason.recoverable() {
            Health::Recovering { attempt: 0, reason }
        } else {
            Health::Terminal { reason }
        };
    }
}

/// A request handed from a caller to the driver, with a one-shot reply channel.
struct Command {
    method: String,
    params: Value,
    reply: Sender<Result<Value, SupervisorError>>,
}

// ---------------------------------------------------------------------------
// Supervisor
// ---------------------------------------------------------------------------

/// Owns the driver + monitor threads. Dropping it shuts both down and kills the
/// child (even a hung one, via the shared `Killer`).
pub struct Supervisor {
    cmd_tx: Option<Sender<Command>>,
    shared: Arc<Mutex<Shared>>,
    driver: Option<JoinHandle<()>>,
    monitor: Option<JoinHandle<()>>,
    cfg: WatchdogConfig,
}

impl Supervisor {
    /// Start supervising. `spawn_fn` produces a fresh [`Sidecar`] on demand — used
    /// once now and again on every restart, so it must capture whatever it needs
    /// (interpreter path, script path). Fails only if the *first* spawn fails.
    pub fn start<F>(spawn_fn: F, cfg: WatchdogConfig, safe_state_path: impl Into<PathBuf>) -> Result<Self, SidecarError>
    where
        F: Fn() -> Result<Sidecar, SidecarError> + Send + 'static,
    {
        let sidecar = spawn_fn()?;
        let shared = Arc::new(Mutex::new(Shared {
            health: Health::Running,
            in_flight: None,
            killer: None,
            generation: 0,
            safe_state_path: safe_state_path.into(),
            shutdown: false,
            manual_retry: false,
            recoveries: 0,
        }));
        // Watch the initial process for exit (event-based, §7.1); this also sets the
        // kill handle and generation.
        install_waiter(&shared, sidecar.killer());
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();

        let driver = {
            let (shared, cfg) = (Arc::clone(&shared), cfg.clone());
            thread::spawn(move || driver_loop(spawn_fn, sidecar, cmd_rx, shared, cfg))
        };
        let monitor = {
            let (shared, cfg) = (Arc::clone(&shared), cfg.clone());
            thread::spawn(move || monitor_loop(shared, cfg))
        };

        Ok(Supervisor { cmd_tx: Some(cmd_tx), shared, driver: Some(driver), monitor: Some(monitor), cfg })
    }

    /// Submit one request and block for its reply. Fails fast if the watchdog is not
    /// `Running` rather than touching a dead/absent sidecar.
    pub fn call(&self, method: &str, params: Value) -> Result<Value, SupervisorError> {
        match self.health() {
            Health::Running => {}
            Health::Recovering { reason, .. } => return Err(SupervisorError::Recovering(reason)),
            Health::Terminal { reason } => return Err(SupervisorError::Tripped(reason)),
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        let cmd = Command { method: method.to_string(), params, reply: reply_tx };
        self.cmd_tx
            .as_ref()
            .ok_or(SupervisorError::DriverGone)?
            .send(cmd)
            .map_err(|_| SupervisorError::DriverGone)?;

        match reply_rx.recv_timeout(self.cfg.busy_response + self.cfg.tick * 2) {
            Ok(result) => result,
            // The monitor should have tripped by now; report the current state.
            Err(_) => Err(match self.health() {
                Health::Terminal { reason } => SupervisorError::Tripped(reason),
                Health::Recovering { reason, .. } => SupervisorError::Recovering(reason),
                Health::Running => SupervisorError::DriverGone,
            }),
        }
    }

    /// Current health (cloned; lock released).
    pub fn health(&self) -> Health {
        self.shared.lock().unwrap().health.clone()
    }

    /// How many times the sidecar has been successfully restarted.
    pub fn recoveries(&self) -> u32 {
        self.shared.lock().unwrap().recoveries
    }

    /// Request a single manual restart attempt out of `Terminal` (§7.2 "Erneut
    /// versuchen"). No-op if not terminal.
    pub fn retry(&self) {
        self.shared.lock().unwrap().manual_retry = true;
    }

    /// Block until `pred(health)` holds or `timeout` elapses; returns the last
    /// health seen. Introspection/test aid.
    pub fn wait_until(&self, mut pred: impl FnMut(&Health) -> bool, timeout: Duration) -> Health {
        let deadline = Instant::now() + timeout;
        loop {
            let h = self.health();
            if pred(&h) || Instant::now() >= deadline {
                return h;
            }
            thread::sleep(self.cfg.tick);
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        // Signal shutdown, then kill the current child so a driver blocked in a read
        // (a real hang) unblocks and can exit — the shared Killer is what makes this
        // possible. Finally drop the sender so an idle driver's recv also returns.
        let killer = {
            let mut s = self.shared.lock().unwrap();
            s.shutdown = true;
            s.killer.clone()
        };
        self.cmd_tx.take();
        if let Some(k) = killer {
            let _ = k.kill();
        }
        if let Some(h) = self.monitor.take() {
            let _ = h.join();
        }
        if let Some(h) = self.driver.take() {
            let _ = h.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Driver loop
// ---------------------------------------------------------------------------

fn driver_loop<F>(spawn_fn: F, mut sidecar: Sidecar, cmd_rx: Receiver<Command>, shared: Arc<Mutex<Shared>>, cfg: WatchdogConfig)
where
    F: Fn() -> Result<Sidecar, SidecarError>,
{
    // Warm-up: a freshly spawned interpreter pays a one-time import cost (numpy/scipy/
    // autoeq, ~0.4 s) on its first request. Pay it here — this thread starts during app
    // setup, before the window is shown — so the import overlaps webview boot instead of
    // landing on the user's first real request (the catalogue load / initial re-fit).
    // No `begin`, so the monitor ignores it (a slow import must not look like a hang);
    // a genuine failure still feeds the normal fault path, exactly like the heartbeat.
    // The respawn path already confirms replacements with a ping (`run_recovery`); this
    // gives the initial spawn the same warming round-trip.
    if !shared.lock().unwrap().shutdown {
        let outcome = classify(&sidecar.ping());
        if !shared.lock().unwrap().shutdown {
            after_interaction(outcome, &spawn_fn, &mut sidecar, &shared, &cfg);
        }
    }

    loop {
        let health = {
            let s = shared.lock().unwrap();
            if s.shutdown {
                break;
            }
            s.health.clone()
        };
        match health {
            Health::Terminal { .. } => {
                // Idle until a manual retry or shutdown.
                let retry = {
                    let mut s = shared.lock().unwrap();
                    std::mem::take(&mut s.manual_retry)
                };
                if retry {
                    manual_retry(&spawn_fn, &mut sidecar, &shared);
                } else {
                    thread::sleep(cfg.tick);
                }
                continue;
            }
            // Defensive: normally recovery is driven inline from after_interaction;
            // this only fires if we somehow observe Recovering at the top.
            Health::Recovering { .. } => {
                run_recovery(&spawn_fn, &mut sidecar, &shared, &cfg);
                continue;
            }
            Health::Running => {}
        }

        match cmd_rx.recv_timeout(cfg.idle_interval) {
            Ok(cmd) => {
                begin(&shared, Mode::Busy, cfg.busy_response);
                let r = sidecar.call(&cmd.method, cmd.params);
                let outcome = classify(&r);
                let _ = cmd.reply.send(r.map_err(SupervisorError::from)); // ignore if caller left
                if shared.lock().unwrap().shutdown {
                    break;
                }
                after_interaction(outcome, &spawn_fn, &mut sidecar, &shared, &cfg);
            }
            Err(RecvTimeoutError::Timeout) => {
                // No work within idle_interval -> heartbeat.
                begin(&shared, Mode::Idle, cfg.idle_response);
                let r = sidecar.ping();
                let outcome = classify(&r);
                if shared.lock().unwrap().shutdown {
                    break;
                }
                after_interaction(outcome, &spawn_fn, &mut sidecar, &shared, &cfg);
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    // `sidecar` drops here: its Drop kills the child and joins the stderr pump.
}

/// Record that the interaction started (start + deadline for the monitor).
fn begin(shared: &Arc<Mutex<Shared>>, mode: Mode, deadline: Duration) {
    let mut s = shared.lock().unwrap();
    if matches!(s.health, Health::Running) {
        s.in_flight = Some(InFlight { mode, started: Instant::now(), deadline });
    }
}

/// Clear the in-flight mark, trip if the interaction revealed a fault, and drive
/// recovery if that left us `Recovering`.
fn after_interaction<F>(outcome: Outcome, spawn_fn: &F, sidecar: &mut Sidecar, shared: &Arc<Mutex<Shared>>, cfg: &WatchdogConfig)
where
    F: Fn() -> Result<Sidecar, SidecarError>,
{
    let recovering = {
        let mut s = shared.lock().unwrap();
        let waited = s.in_flight.as_ref().map(|i| i.started.elapsed()).unwrap_or_default();
        let mode = s.in_flight.as_ref().map(|i| i.mode).unwrap_or(Mode::Busy);
        s.in_flight = None;
        match outcome {
            Outcome::Ok => {}
            Outcome::Exited => s.trip_if_running(TripReason::SidecarExited),
            Outcome::Failed => s.trip_if_running(TripReason::Unresponsive { mode, waited }),
        }
        matches!(s.health, Health::Recovering { .. })
    };
    if recovering {
        run_recovery(spawn_fn, sidecar, shared, cfg);
    }
}

/// Automatic restart with backoff (§7.2). Runs the schedule; on the first fresh
/// sidecar that answers a ping, returns to `Running`; if all attempts fail, moves
/// to `Terminal`. Bails immediately on shutdown.
fn run_recovery<F>(spawn_fn: &F, sidecar: &mut Sidecar, shared: &Arc<Mutex<Shared>>, cfg: &WatchdogConfig)
where
    F: Fn() -> Result<Sidecar, SidecarError>,
{
    let reason = match &shared.lock().unwrap().health {
        Health::Recovering { reason, .. } => reason.clone(),
        _ => return,
    };
    for (i, backoff) in cfg.restart_backoffs.iter().enumerate() {
        {
            let mut s = shared.lock().unwrap();
            if s.shutdown {
                return;
            }
            s.health = Health::Recovering { attempt: i as u32 + 1, reason: reason.clone() };
        }
        if interruptible_sleep(shared, *backoff, cfg.tick) {
            return; // shutdown during backoff
        }
        if try_respawn(spawn_fn, sidecar, shared) {
            return; // recovered -> Running
        }
    }
    let mut s = shared.lock().unwrap();
    if !s.shutdown {
        s.health = Health::Terminal { reason };
    }
}

/// One manual restart attempt out of `Terminal` (no backoff).
fn manual_retry<F>(spawn_fn: &F, sidecar: &mut Sidecar, shared: &Arc<Mutex<Shared>>)
where
    F: Fn() -> Result<Sidecar, SidecarError>,
{
    let reason = {
        let mut s = shared.lock().unwrap();
        match &s.health {
            Health::Terminal { reason } => {
                let r = reason.clone();
                s.health = Health::Recovering { attempt: 1, reason: r.clone() };
                r
            }
            _ => return,
        }
    };
    if !try_respawn(spawn_fn, sidecar, shared) {
        let mut s = shared.lock().unwrap();
        if !s.shutdown {
            s.health = Health::Terminal { reason };
        }
    }
}

/// Spawn a replacement sidecar and confirm it with a ping. On success: swap it in,
/// update the kill handle, mark `Running`, bump the recovery count, return true.
fn try_respawn<F>(spawn_fn: &F, sidecar: &mut Sidecar, shared: &Arc<Mutex<Shared>>) -> bool
where
    F: Fn() -> Result<Sidecar, SidecarError>,
{
    let fresh = match spawn_fn() {
        Ok(s) => s,
        Err(_) => return false, // couldn't even start one
    };
    let killer = fresh.killer();
    *sidecar = fresh; // dropping the old Sidecar kills the old (crashed/hung) process
    // Watch the new process; bumps the generation so the old, now-dead process's
    // waiter fires harmlessly (generation mismatch).
    install_waiter(shared, killer);

    // A fresh, ping-answering process is a sufficiently safe recovery signal (§7.2).
    match sidecar.ping() {
        Ok(()) => {
            let mut s = shared.lock().unwrap();
            s.health = Health::Running;
            s.in_flight = None;
            s.recoveries += 1;
            true
        }
        Err(_) => false,
    }
}

/// Sleep `dur`, but wake every `tick` to notice shutdown. Returns true if shutdown.
fn interruptible_sleep(shared: &Arc<Mutex<Shared>>, dur: Duration, tick: Duration) -> bool {
    let deadline = Instant::now() + dur;
    loop {
        if shared.lock().unwrap().shutdown {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        thread::sleep(tick.min(deadline - now));
    }
}

/// Record a newly-(re)spawned sidecar as current — bump the generation, store its
/// kill handle — and spawn a thread that blocks on its OS process handle and trips
/// **the instant** it exits (§7.1's event-based detection; no polling, no waiting
/// for the next heartbeat). The generation it captures, plus the `Running` guard in
/// `trip_if_running`, make the waiter of a replaced or intentionally-killed process
/// a no-op, so only a genuine crash of the *current* sidecar trips.
fn install_waiter(shared: &Arc<Mutex<Shared>>, killer: cageq_sidecar::Killer) {
    let generation = {
        let mut s = shared.lock().unwrap();
        s.generation += 1;
        s.killer = Some(killer.clone());
        s.generation
    };
    let shared = Arc::clone(shared);
    thread::spawn(move || {
        let _ = killer.wait(); // blocks until THIS process exits
        let mut s = shared.lock().unwrap();
        if !s.shutdown && s.generation == generation {
            s.trip_if_running(TripReason::SidecarExited);
        }
    });
}

// ---------------------------------------------------------------------------
// Monitor loop
// ---------------------------------------------------------------------------

/// Owns only a clock. Trips a blown deadline and kills the sidecar to unblock the
/// driver. Runs for the whole lifetime, supervising across recoveries.
fn monitor_loop(shared: Arc<Mutex<Shared>>, cfg: WatchdogConfig) {
    loop {
        thread::sleep(cfg.tick);
        let killer = {
            let mut s = shared.lock().unwrap();
            if s.shutdown {
                break;
            }
            // Only trip from Running; during Recovering/Terminal the driver owns
            // all transitions.
            if !matches!(s.health, Health::Running) {
                continue;
            }
            let breach = s
                .in_flight
                .as_ref()
                .and_then(|op| (op.started.elapsed() > op.deadline).then(|| (op.mode, op.started.elapsed())));
            match breach {
                Some((mode, waited)) => {
                    s.trip_if_running(TripReason::Unresponsive { mode, waited });
                    s.killer.clone() // kill outside the lock, below
                }
                None => continue,
            }
        };
        // Killing closes the child's pipes, so the driver's blocked read returns EOF
        // and it proceeds into recovery.
        if let Some(k) = killer {
            let _ = k.kill();
        }
    }
}

// ---------------------------------------------------------------------------
// Interaction classification
// ---------------------------------------------------------------------------

/// A completed interaction. A *remote* error (sidecar answered but refused) is
/// healthy and must not trip.
enum Outcome {
    Ok,
    Exited,
    Failed,
}

fn classify<T>(r: &Result<T, SidecarError>) -> Outcome {
    match r {
        Ok(_) => Outcome::Ok,
        Err(SidecarError::Exited) => Outcome::Exited,
        Err(SidecarError::Remote { .. }) => Outcome::Ok, // answered, just said no
        Err(_) => Outcome::Failed,                       // Io/Json/Protocol: transport broke
    }
}

/// The cageq.txt path convention, for callers wiring the supervisor to a real EqAPO
/// config directory.
pub fn safe_state_path_in(config_dir: &Path) -> PathBuf {
    config_dir.join(cageq_config_writer::CAGEQ_FILENAME)
}
