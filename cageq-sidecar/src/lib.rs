//! Python sidecar transport — the Rust core's client for the DSP engine
//! ("IPC-Transport": persistent child process, line-delimited
//! JSON-RPC on stdin/stdout, no network port).
//!
//! What this crate owns:
//!   * spawning the Python interpreter on a script and keeping it alive,
//!   * one synchronous `call(method, params) -> result` over stdio,
//!   * draining the child's stderr so it can never wedge the pipe,
//!   * killing the child on `Drop` so we never orphan a `python.exe`.
//!
//! What it deliberately does NOT own:
//!   * timeouts / liveness. If the sidecar hangs, `call` blocks, and nothing else steps in.
//!     The app no longer runs the sidecar at all (the fit, the catalogue and the export moved
//!     to Rust); it is here for the tests that hold the Rust ports to the live `autoeq`
//!     package, where a hung test is the signal.
//!   * the real DSP. We talk to `python/sidecar_stub.py`, a stand-in that speaks
//!     the same protocol with canned answers. The real AutoEq engine replaces
//!     the stub's method bodies, not this code.
//!
//! Two buffering traps worth internalising (both bit real stdio pipelines):
//!   1. OS-pipe deadlock: a pipe has a fixed kernel buffer. If the child writes
//!      to stderr while we're blocked reading stdout, its stderr write eventually
//!      blocks, and if it blocks before sending our stdout reply, both sides
//!      hang. Fix: a dedicated thread that continuously drains stderr.
//!   2. Language-level buffering: CPython block-buffers stdout when it's a pipe
//!      (not a TTY), so a reply can sit unflushed and our `read_line` hangs. Fix,
//!      belt and suspenders: launch python with `-u` (unbuffered) AND have the
//!      script flush after every reply.
//!
//! Concurrency model: strictly one request at a time. `call` takes `&mut self`,
//! so the borrow checker makes "two overlapping requests" a compile error rather
//! than a runtime race — the type system encodes the protocol invariant for free.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use shared_child::SharedChild;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Everything that can go wrong talking to the sidecar. Same `thiserror` shape as
/// the config-writer's `WriteError`: one enum, `#[from]` for the plumbing errors
/// we just want to bubble up with `?`, explicit variants for the ones we detect.
#[derive(Debug, thiserror::Error)]
pub enum SidecarError {
    /// The interpreter/script could not even be started (bad path, not
    /// executable). Kept distinct from `Io` so "sidecar never launched" reads
    /// differently from "sidecar died mid-conversation".
    #[error("failed to spawn the sidecar process")]
    Spawn(#[source] std::io::Error),
    /// An I/O error while writing a request or reading a reply.
    #[error("I/O error talking to the sidecar")]
    Io(#[from] std::io::Error),
    /// A request/reply could not be (de)serialized as JSON.
    #[error("could not (de)serialize a JSON-RPC message")]
    Json(#[from] serde_json::Error),
    /// The child closed its stdout (EOF) instead of replying — it exited/crashed.
    #[error("the sidecar exited before answering")]
    Exited,
    /// The reply broke the contract (wrong id, or neither result nor error).
    #[error("sidecar protocol violation: {0}")]
    Protocol(String),
    /// A well-formed JSON-RPC *error* object came back — the sidecar ran but
    /// refused the request (unknown method, bad params, internal failure).
    #[error("sidecar returned error {code}: {message}")]
    Remote { code: i64, message: String },
}

// ---------------------------------------------------------------------------
// Wire format — JSON-RPC 2.0, one object per line
// ---------------------------------------------------------------------------

/// A request we send. `#[derive(Serialize)]` writes the JSON; field names become
/// object keys. It borrows `method` (`&'a str`) so a `call("ping", ...)` needs no
/// allocation for the method name.
#[derive(Serialize)]
struct Request<'a> {
    jsonrpc: &'static str, // always "2.0"
    id: u64,
    method: &'a str,
    params: Value,
}

/// A reply we receive. `#[serde(default)]` makes each of `result`/`error` optional:
/// a success reply omits `error`, an error reply omits `result`, and a missing key
/// deserializes to `None` instead of failing. Any extra keys (like `jsonrpc`) are
/// ignored by default.
#[derive(Deserialize)]
struct Response {
    id: u64,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

// ---------------------------------------------------------------------------
// The sidecar handle
// ---------------------------------------------------------------------------

/// A live sidecar process plus the plumbing to talk to it. Owns the child, so
/// when this value drops the process is killed (see the `Drop` impl).
pub struct Sidecar {
    proc: Arc<SharedChild>,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    // `Option` so `Drop` can `take()` the handle out and `join()` it. Kept only to
    // reap the stderr thread cleanly on shutdown.
    stderr_pump: Option<JoinHandle<()>>,
}

impl Sidecar {
    /// Launch `python -u <script>` and wire up its stdio. `python` is the
    /// interpreter path (resolve the real `python.exe`, not a launcher, so `Drop`
    /// kills the actual process); `script` is the sidecar entry point.
    pub fn spawn(python: &Path, script: &Path) -> Result<Self, SidecarError> {
        let mut cmd = Command::new(python);
        cmd.arg("-u") // unbuffered stdio: replies flush immediately (trap #2)
            .arg(script);
        Self::spawn_command(cmd)
    }

    /// Shared spawn path: pipe stdio, wrap in `SharedChild`, and start the stderr pump.
    fn spawn_command(mut cmd: Command) -> Result<Self, SidecarError> {
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());

        // SharedChild wraps the process so kill()/wait() work through a shared OS handle,
        // which `Drop` uses to kill a child still mid-call.
        let proc = Arc::new(SharedChild::spawn(&mut cmd).map_err(SidecarError::Spawn)?);

        // Take the piped handles out of the shared process; guaranteed Some right
        // after a piped spawn, so the ok_or maps the impossible None.
        let stdin = proc.take_stdin().ok_or_else(|| miswired("stdin"))?;
        let stdout = proc.take_stdout().ok_or_else(|| miswired("stdout"))?;
        let stderr = proc.take_stderr().ok_or_else(|| miswired("stderr"))?;

        // Drain stderr forever on its own thread (trap #1). `move` transfers
        // ownership of `stderr` into the closure; the thread ends on its own when
        // the child dies and the pipe hits EOF.
        let stderr_pump = thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                match line {
                    Ok(l) => eprintln!("[sidecar] {l}"),
                    Err(_) => break,
                }
            }
        });

        Ok(Sidecar {
            proc,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            stderr_pump: Some(stderr_pump),
        })
    }

    /// Send one request and block for its reply. Increments the id, writes the
    /// request as a single `\n`-terminated line, flushes, then reads exactly one
    /// reply line. Returns the `result` value, or an error for an EOF / id
    /// mismatch / remote error object.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, SidecarError> {
        let id = self.next_id;
        self.next_id += 1;

        // --- send ---
        let mut line = serde_json::to_string(&Request { jsonrpc: "2.0", id, method, params })?;
        line.push('\n'); // the delimiter the reader on the other side splits on
        self.stdin.write_all(line.as_bytes())?;
        self.stdin.flush()?;

        // --- receive ---
        // Until this call's own reply. A line that isn't a reply (a library printing to stdout)
        // and a reply to an earlier call (left in the pipe when that call failed on such a line)
        // are skipped: failing on them left every later call reading the one before's reply,
        // the sidecar unusable for good.
        let resp = loop {
            let mut buf = String::new();
            // read_line returns Ok(0) only at EOF: the child closed stdout, i.e. exited.
            if self.stdout.read_line(&mut buf)? == 0 {
                return Err(SidecarError::Exited);
            }
            let Ok(resp) = serde_json::from_str::<Response>(buf.trim_end()) else {
                eprintln!("[sidecar] skipping a stdout line that isn't a reply: {}", buf.trim_end());
                continue;
            };
            if resp.id < id {
                continue;
            }
            // A reply to a request not yet sent is a stream out of step: an error, not a skip.
            if resp.id != id {
                return Err(SidecarError::Protocol(format!("reply id {} != request id {id}", resp.id)));
            }
            break resp;
        };
        match (resp.result, resp.error) {
            (_, Some(e)) => Err(SidecarError::Remote { code: e.code, message: e.message }),
            (Some(v), None) => Ok(v),
            (None, None) => Err(SidecarError::Protocol("reply had neither result nor error".into())),
        }
    }

    /// Liveness probe: `ping` -> expect `{"pong": true}`. A cheap way to confirm
    /// the process launched and is answering before doing real work.
    pub fn ping(&mut self) -> Result<(), SidecarError> {
        let v = self.call("ping", Value::Null)?;
        if v.get("pong").and_then(Value::as_bool) == Some(true) {
            Ok(())
        } else {
            Err(SidecarError::Protocol(format!("unexpected ping reply: {v}")))
        }
    }

    /// Politely ask the sidecar to exit, then reap it. Best-effort: if it has
    /// already gone, the `Drop` kill/wait below still cleans up. Consumes `self`
    /// so the handle can't be used afterwards.
    pub fn shutdown(mut self) {
        let _ = self.call("shutdown", Value::Null);
        // falling out of scope runs Drop, which kills (if needed) and waits.
    }
}

impl Drop for Sidecar {
    /// RAII teardown. A destructor must never panic, so every step is best-effort.
    /// Killing the child closes its pipes, which lets the stderr pump reach EOF and
    /// end; we then join it so no thread outlives the handle.
    fn drop(&mut self) {
        let _ = self.proc.kill();
        let _ = self.proc.wait(); // reap the zombie; also unblocks the pump's read
        if let Some(h) = self.stderr_pump.take() {
            let _ = h.join();
        }
    }
}

/// The "piped handle was unexpectedly None" case — impossible right after a piped
/// spawn, but we surface it as a protocol error rather than unwrap-panic.
fn miswired(which: &str) -> SidecarError {
    SidecarError::Protocol(format!("child {which} handle was not captured"))
}
