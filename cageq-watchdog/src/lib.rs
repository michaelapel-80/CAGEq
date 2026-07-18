//! Fail-safe watchdog / sidecar supervisor (filter.md §7.1).
//!
//! The premise is "better no sound than wrong sound": the moment the DSP sidecar
//! stops behaving, we drive Equalizer APO into a hardcoded silent safe state
//! (`Device: all` + `Preamp: -120.0 dB`) written *by Rust*, independently of the
//! (possibly dead) Python process. That last part is the whole point — the most
//! likely failure is the sidecar itself crashing/hanging, so the thing that
//! reacts must not live inside it.
//!
//! ## Why two threads
//! The failure we must survive is "the sidecar hung and the driver is blocked in a
//! read." Whatever enforces the timeout therefore cannot be the blocked thread.
//! So:
//!   * a **driver** thread owns the [`Sidecar`], runs one request at a time, and —
//!     when no request is pending — emits an idle heartbeat ping;
//!   * a **monitor** thread owns nothing but a clock and the shared state, and
//!     trips when a deadline is blown or the sidecar has exited.
//!
//! They coordinate through an `Arc<Mutex<State>>`: the driver records "an op
//! started at T with deadline D" before each call and clears it after; the monitor
//! reads that and fires if `T + D` passes with the op still outstanding.
//!
//! ## Two-mode timeout (§7.1)
//! A single fixed heartbeat would false-trip during a legitimately slow AutoEq
//! fit, so there are two deadlines:
//!   * **idle** — no request in flight: ping every `idle_interval` (5 s), answer
//!     within `idle_response` (2 s);
//!   * **busy** — a real request in flight: give it `busy_response` (15 s), a
//!     comfortable margin over the fit.
//!
//! ## What a trip does — and deliberately does NOT do
//! A trip writes the safe state and latches a [`TripReason`]. It does **not** kill
//! or restart the sidecar. Killing a hung child cross-thread and the restart
//! policy (§7.2: 3 tries, 2/5/10 s backoff, auto-leave on a fresh heartbeat) are
//! the next slice; safety here is achieved purely by making the output silent, so
//! that harder machinery isn't on the safety-critical path. A genuinely wedged
//! sidecar therefore leaves the driver thread blocked until it eventually returns
//! (or the process exits) — acceptable because silence is already guaranteed.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cageq_config_writer::write_safe_state;
use cageq_sidecar::{Sidecar, SidecarError};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Configuration and outcomes
// ---------------------------------------------------------------------------

/// The two deadline regimes plus the monitor's poll granularity. Defaults are the
/// §7.1 values; tests override them with much smaller ones so a run takes ms.
#[derive(Debug, Clone)]
pub struct WatchdogConfig {
    /// Idle: how often to send a heartbeat ping when nothing else is happening.
    pub idle_interval: Duration,
    /// Idle: a heartbeat ping must answer within this.
    pub idle_response: Duration,
    /// Busy: a real request must answer within this.
    pub busy_response: Duration,
    /// How often the monitor thread re-checks the deadline.
    pub tick: Duration,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            idle_interval: Duration::from_secs(5),
            idle_response: Duration::from_secs(2),
            busy_response: Duration::from_secs(15),
            tick: Duration::from_millis(200),
        }
    }
}

/// Which deadline regime an operation was under when it was measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Idle,
    Busy,
}

/// Why the watchdog tripped to the safe state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TripReason {
    /// The sidecar process ended (EOF on its stdout) — a crash or external kill.
    SidecarExited,
    /// A request or heartbeat blew its deadline without answering.
    Unresponsive { mode: Mode, waited: Duration },
    /// The trip fired but writing the safe state itself failed — the gravest case:
    /// we could not even guarantee silence. Carries the writer's error text.
    SafeStateWriteFailed(String),
}

/// Errors surfaced to a caller of [`Supervisor::call`].
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    /// The watchdog has tripped; no further work is accepted until recovery.
    #[error("watchdog tripped to safe state: {0:?}")]
    Tripped(TripReason),
    /// The driver thread is gone (shutting down). Shouldn't happen in normal use.
    #[error("sidecar driver is no longer running")]
    DriverGone,
    /// A transport/remote error from the sidecar for this specific request. A
    /// remote error (e.g. unknown method) is passed through here and does NOT trip.
    #[error("sidecar error: {0}")]
    Sidecar(#[from] SidecarError),
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// One outstanding operation the monitor is timing.
struct InFlight {
    mode: Mode,
    started: Instant,
    deadline: Duration,
}

/// State shared between the driver and the monitor. All access is behind one
/// mutex; there is no lock nesting, so it cannot deadlock.
struct State {
    in_flight: Option<InFlight>,
    exited: bool,
    tripped: Option<TripReason>,
    shutdown: bool,
    safe_state_path: PathBuf,
}

impl State {
    /// Idempotent trip: on the first call, write the safe state, latch the reason,
    /// and request shutdown; later calls are no-ops. Writing under the lock keeps
    /// the driver and monitor from both writing the file at once.
    fn trip(&mut self, reason: TripReason) {
        if self.tripped.is_some() {
            return;
        }
        let reason = match write_safe_state(&self.safe_state_path) {
            Ok(()) => reason,
            // If we can't even write silence, that supersedes the original reason.
            Err(e) => TripReason::SafeStateWriteFailed(e.to_string()),
        };
        self.tripped = Some(reason);
        self.shutdown = true; // wind the driver down after its current op
    }
}

/// A request handed from a caller to the driver thread, with a one-shot reply
/// channel back.
struct Command {
    method: String,
    params: Value,
    reply: Sender<Result<Value, SupervisorError>>,
}

// ---------------------------------------------------------------------------
// Supervisor
// ---------------------------------------------------------------------------

/// Owns the driver + monitor threads and the channel to submit requests. Dropping
/// it shuts both threads down and (via `Sidecar`'s own `Drop`) kills the child.
pub struct Supervisor {
    // Option so Drop can drop the sender early, which unblocks the driver's
    // recv_timeout with `Disconnected`.
    cmd_tx: Option<Sender<Command>>,
    state: Arc<Mutex<State>>,
    driver: Option<JoinHandle<()>>,
    monitor: Option<JoinHandle<()>>,
    cfg: WatchdogConfig,
}

impl Supervisor {
    /// Take ownership of a spawned [`Sidecar`] and start supervising it. `safe_state_path`
    /// is the cageq.txt the app already had EqAPO `Include:`, so a trip's write
    /// takes effect immediately.
    pub fn start(sidecar: Sidecar, cfg: WatchdogConfig, safe_state_path: impl Into<PathBuf>) -> Self {
        let state = Arc::new(Mutex::new(State {
            in_flight: None,
            exited: false,
            tripped: None,
            shutdown: false,
            safe_state_path: safe_state_path.into(),
        }));
        let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();

        let driver = {
            let (state, cfg) = (Arc::clone(&state), cfg.clone());
            thread::spawn(move || driver_loop(sidecar, cmd_rx, state, cfg))
        };
        let monitor = {
            let (state, cfg) = (Arc::clone(&state), cfg.clone());
            thread::spawn(move || monitor_loop(state, cfg))
        };

        Supervisor { cmd_tx: Some(cmd_tx), state, driver: Some(driver), monitor: Some(monitor), cfg }
    }

    /// Submit one request and block for its reply (up to the busy deadline). If the
    /// watchdog has tripped, fails fast with [`SupervisorError::Tripped`] instead of
    /// touching a possibly-dead sidecar.
    pub fn call(&self, method: &str, params: Value) -> Result<Value, SupervisorError> {
        if let Some(reason) = self.tripped() {
            return Err(SupervisorError::Tripped(reason));
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        let cmd = Command { method: method.to_string(), params, reply: reply_tx };
        self.cmd_tx
            .as_ref()
            .ok_or(SupervisorError::DriverGone)?
            .send(cmd)
            .map_err(|_| SupervisorError::DriverGone)?;

        // Wait a hair past the busy deadline so the monitor gets to trip first and
        // we can report *why* rather than a bare timeout.
        match reply_rx.recv_timeout(self.cfg.busy_response + self.cfg.tick * 2) {
            Ok(result) => result,
            Err(_) => Err(SupervisorError::Tripped(self.tripped().unwrap_or(
                TripReason::Unresponsive { mode: Mode::Busy, waited: self.cfg.busy_response },
            ))),
        }
    }

    /// The current trip reason, if any. Cloned out so the lock isn't held.
    pub fn tripped(&self) -> Option<TripReason> {
        self.state.lock().unwrap().tripped.clone()
    }

    /// Block until a trip occurs or `timeout` elapses. Mostly a test/introspection
    /// aid; polls at the monitor's tick.
    pub fn wait_for_trip(&self, timeout: Duration) -> Option<TripReason> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(reason) = self.tripped() {
                return Some(reason);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(self.cfg.tick);
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        // Ask both threads to wind down: set the flag, and drop the command sender
        // so an idle driver's recv_timeout returns Disconnected immediately.
        if let Ok(mut s) = self.state.lock() {
            s.shutdown = true;
        }
        self.cmd_tx.take();
        if let Some(h) = self.monitor.take() {
            let _ = h.join();
        }
        // NOTE: if the sidecar is genuinely wedged, the driver is blocked in a read
        // and this join waits until it returns. Bounding that (force-kill) is the
        // §7.2 recovery slice; here the bounded test stub always returns.
        if let Some(h) = self.driver.take() {
            let _ = h.join();
        }
    }
}

// ---------------------------------------------------------------------------
// The two loops
// ---------------------------------------------------------------------------

/// Owns the sidecar; serves one command at a time and heartbeats when idle.
fn driver_loop(mut sidecar: Sidecar, cmd_rx: Receiver<Command>, state: Arc<Mutex<State>>, cfg: WatchdogConfig) {
    loop {
        if state.lock().unwrap().shutdown {
            break;
        }
        match cmd_rx.recv_timeout(cfg.idle_interval) {
            Ok(cmd) => {
                begin(&state, Mode::Busy, cfg.busy_response);
                let r = sidecar.call(&cmd.method, cmd.params);
                note(&state, classify(&r));
                let _ = cmd.reply.send(r.map_err(SupervisorError::from)); // ignore if caller left
            }
            Err(RecvTimeoutError::Timeout) => {
                // No work arrived within idle_interval -> heartbeat.
                begin(&state, Mode::Idle, cfg.idle_response);
                let r = sidecar.ping();
                note(&state, classify(&r));
            }
            Err(RecvTimeoutError::Disconnected) => break, // Supervisor dropped
        }
    }
    // `sidecar` drops here: its own Drop kills the child and joins the stderr pump.
}

/// Owns only a clock; trips when a deadline is blown or the sidecar has exited.
fn monitor_loop(state: Arc<Mutex<State>>, cfg: WatchdogConfig) {
    loop {
        thread::sleep(cfg.tick);
        let mut s = state.lock().unwrap();
        if s.tripped.is_some() || s.shutdown {
            break;
        }
        // Compute the breach from the immutable borrow first, then release it before
        // the mutable `trip` call (can't hold both borrows of `s` at once).
        let breach = s
            .in_flight
            .as_ref()
            .and_then(|op| (op.started.elapsed() > op.deadline).then(|| (op.mode, op.started.elapsed())));
        if let Some((mode, waited)) = breach {
            s.trip(TripReason::Unresponsive { mode, waited });
            break;
        }
        if s.exited {
            s.trip(TripReason::SidecarExited);
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// State transitions shared by the driver
// ---------------------------------------------------------------------------

/// Classification of a completed sidecar interaction. A *remote* error (the sidecar
/// answered but refused the request) is healthy — it must not trip.
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

/// Mark the operation begun (records start + deadline for the monitor).
fn begin(state: &Arc<Mutex<State>>, mode: Mode, deadline: Duration) {
    let mut s = state.lock().unwrap();
    if s.tripped.is_none() {
        s.in_flight = Some(InFlight { mode, started: Instant::now(), deadline });
    }
}

/// Mark the operation finished and trip if it revealed a fault.
fn note(state: &Arc<Mutex<State>>, outcome: Outcome) {
    let mut s = state.lock().unwrap();
    let inflight = s.in_flight.take();
    match outcome {
        Outcome::Ok => {}
        Outcome::Exited => {
            s.exited = true;
            s.trip(TripReason::SidecarExited);
        }
        Outcome::Failed => {
            let (mode, waited) =
                inflight.map(|i| (i.mode, i.started.elapsed())).unwrap_or((Mode::Busy, Duration::ZERO));
            s.trip(TripReason::Unresponsive { mode, waited });
        }
    }
}

/// The cageq.txt path convention, re-exported for callers wiring the supervisor to
/// a real EqAPO config directory.
pub fn safe_state_path_in(config_dir: &Path) -> PathBuf {
    config_dir.join(cageq_config_writer::CAGEQ_FILENAME)
}
