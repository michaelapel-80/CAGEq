import { invoke, Channel } from "@tauri-apps/api/core";
import type { SpectrumData } from "./EqChart";
import type { ScopeData } from "./Vectorscope";
import { ScopeClock, type ScopeReader } from "./scopeClock";

/**
 * Data plane, frontend side. The meter/spectrum/scope streams used to arrive as Tauri
 * *events* (`listen("monitor")` etc.), but Tauri delivers each backend→frontend event by
 * evaluating a script in the webview — at this app's sustained ~120 events/s that churned
 * WebView2's memory at ~100 MiB/min, faster than its GC kept up (the "out of memory" crash
 * class). `Channel` rides the raw IPC pipe instead, the documented transport for streams.
 *
 * Each stream registers **one** channel per webview, lazily on first subscribe, and keeps it
 * for the webview's whole lifetime (unless it wedges — see `makeStream`) —the backend keys its subscriber list by webview label and
 * drops the entry when that window is destroyed (`drop_subs` in src-tauri/src/lib.rs; a dead
 * channel can't be detected from the send side, which is documented there). Components
 * subscribe/unsubscribe *locally* (a Set of callbacks, no IPC),
 * so React unmounts/remounts — including StrictMode's dev double-mount — never re-register.
 * The pop-out scope window only ever subscribes to `scopeStream`, so it registers only that
 * channel; whether the scope stream carries data at all stays gated by `set_scope_viewer`.
 */

/** Mirror of cageq-monitor's `MeterUpdate` (Meter.tsx consumes it; documented there). */
export type MeterData = {
  peak_db: number;
  rms_db: number;
  momentary_lufs: number;
  short_term_lufs: number;
  integrated_lufs: number;
  loudness_range: number;
  true_peak_max_db: number;
  signal: boolean;
  bins: number[];
  sample_rate: number;
};

/**
 * Liveness beat, one per webview (not per stream — the backend keys it by webview label). The
 * backend stops sending to a webview that goes quiet for a few seconds: window *destruction* it
 * detects on its own, but a crashed WebView2 renderer or a wedged main thread leaves the window
 * alive while nothing drains the channel, and payloads over 8 KiB park in a Tauri-internal queue
 * until the webview fetches them. See `StreamSubs` in src-tauri/src/lib.rs.
 *
 * Chromium throttles timers in hidden windows, so a minimised/occluded view stops beating and the
 * backend stops feeding it. That's the wanted behaviour, not a bug — those views aren't painting
 * either (their render loops are rAF-driven) — and the `visibilitychange` beat makes coming back
 * immediate rather than waiting out the next interval.
 */
const HEARTBEAT_MS = 1000;
let beating = false;
function startHeartbeat() {
  if (beating) return;
  beating = true;
  const beat = () => void invoke("stream_heartbeat");
  beat();
  setInterval(beat, HEARTBEAT_MS);
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) beat();
  });
}

/**
 * A visible stream silent for this long is presumed wedged and re-registered. The backend emits
 * every stream at least ~25 Hz whenever the monitor runs (silence included — `signal: false`;
 * the spectrum only when it changed, once per FFT hop), so a multi-second gap while visible is
 * never normal traffic.
 */
const STALL_MS = 3000;
let visibleSince = performance.now();
document.addEventListener("visibilitychange", () => {
  if (!document.hidden) visibleSince = performance.now();
});

// --- Shared-memory rings -------------------------------------------------------------------------
// The audio streams through shared memory, where WebView2 can (src-tauri/src/shared_streams.rs
// documents the rings): the capture thread writes every update into a ring per stream, and the
// page reads what's new each frame — no message per update, so none of the per-message script,
// JSON and fetch a `Channel` costs (at 100 scope windows a second per webview, plus the meter and
// spectrum, the renderer's GC fell behind with the scope pop-outs open). Each stream asks for its
// ring when it's first used, and takes its `Channel` instead if the ring doesn't come.
const RING_MAGIC = 0x42534743;
const RING_VERSION = 1;
const RING_HEADER_BYTES = 64;
/** Updates short of a full lap the page reads back at most (see shared_streams.rs). */
const RING_SLOT_MARGIN = 8;
/** How long after asking the page waits for a ring before using the `Channel` instead, ms. */
const RING_WAIT_MS = 2000;
type RingKind = "scope" | "meter" | "spectrum";
type Ring = { u32: Uint32Array; f32: Float32Array; f64: Float64Array; slots: number; slotBytes: number; read: number };
type SharedBufferEvent = { additionalData?: { kind?: string }; getBuffer(): ArrayBuffer };
type WebViewBridge = { addEventListener(type: "sharedbufferreceived", fn: (e: SharedBufferEvent) => void): void };
const rings: Partial<Record<RingKind, Ring>> = {};
const ringArrived: Partial<Record<RingKind, () => void>> = {};
let ringsListening = false;

/** Ask for `kind`'s ring: `onRing` when it arrives (also after a fallback — the backend stops the
 *  `Channel` for a webview with a ring), `onFallback` if it can't or doesn't within RING_WAIT_MS. */
function requestRing(kind: RingKind, onRing: () => void, onFallback: (why: string) => void): void {
  const bridge = (window as unknown as { chrome?: { webview?: WebViewBridge } }).chrome?.webview;
  if (!bridge) return onFallback("no WebView2 bridge");
  if (!ringsListening) {
    ringsListening = true;
    bridge.addEventListener("sharedbufferreceived", (e) => {
      const k = e.additionalData?.kind as RingKind | undefined;
      if (k !== "scope" && k !== "meter" && k !== "spectrum") return;
      const buf = e.getBuffer();
      const u32 = new Uint32Array(buf);
      if (u32[0] !== RING_MAGIC || u32[1] !== RING_VERSION) return console.warn(`[streams] unknown ${k} ring layout`);
      rings[k] = { u32, f32: new Float32Array(buf), f64: new Float64Array(buf), slots: u32[2], slotBytes: u32[3], read: u32[4] };
      ringArrived[k]?.();
    });
  }
  let settled = false;
  ringArrived[kind] = () => {
    settled = true;
    onRing();
  };
  const fallback = (why: string) => {
    if (settled) return;
    settled = true;
    console.warn(`[streams] ${kind} stream over the Channel: ${why}`);
    onFallback(why);
  };
  invoke("subscribe_shared", { stream: kind }).then(
    () => setTimeout(() => fallback("no buffer arrived"), RING_WAIT_MS),
    (e) => fallback(String(e)),
  );
}

/** Each update written to `r` since the last look, by its slot's byte offset, oldest first. */
function forEachNew(r: Ring, fn: (at: number) => void): void {
  const written = r.u32[4];
  let k = r.read;
  if (((written - k) >>> 0) > r.slots - RING_SLOT_MARGIN) k = (written - (r.slots - RING_SLOT_MARGIN)) >>> 0;
  for (; k !== written; k = (k + 1) >>> 0) {
    const at = RING_HEADER_BYTES + (k % r.slots) * r.slotBytes;
    if (r.u32[at >>> 2] !== ((k + 1) >>> 0)) continue; // being rewritten: lapped
    fn(at);
  }
  r.read = written;
}

/** A meter or spectrum record at `at`: its time and its arrays (views into the ring — copy them). */
function readRecord(r: Ring, at: number): { time: number; arrays: Float32Array[] } {
  const count = r.u32[(at + 4) >>> 2];
  const time = r.f64[(at + 8) >>> 3];
  const arrays: Float32Array[] = [];
  let w = (at + 16) >>> 2;
  for (let i = 0; i < count; i++) {
    const len = r.u32[w];
    arrays.push(r.f32.subarray(w + 1, w + 1 + len));
    w += 1 + len;
  }
  return { time, arrays };
}

const toPairs = (a: Float32Array) => {
  const out: { hz: number; db: number }[] = [];
  for (let i = 0; i + 1 < a.length; i += 2) out.push({ hz: a[i], db: a[i + 1] });
  return out;
};

function decodeMeter(_time: number, a: Float32Array[]): MeterData {
  const h = a[0];
  return {
    peak_db: h[0],
    rms_db: h[1],
    momentary_lufs: h[2],
    short_term_lufs: h[3],
    integrated_lufs: h[4],
    loudness_range: h[5],
    true_peak_max_db: h[6],
    signal: h[7] === 1,
    sample_rate: h[8],
    bins: Array.from(a[1]),
  };
}

function decodeSpectrum(time: number, a: Float32Array[]): SpectrumData {
  const h = a[0];
  return {
    signal: h[0] === 1,
    f_min: h[1],
    f_max: h[2],
    peak_separation_octaves: h[3],
    db: Array.from(a[1]),
    peak_db: Array.from(a[2]),
    peak_hz: Array.from(a[3]),
    db_raw: Array.from(a[4]),
    db_lin: Array.from(a[5]),
    peak_db_lin: Array.from(a[6]),
    peak_hz_lin: Array.from(a[7]),
    db_lin_raw: Array.from(a[8]),
    peaks: toPairs(a[9]),
    peak_candidates: toPairs(a[10]),
    t_end: time,
  };
}

// The record streams' readers, run once a frame (and by `spectrumAt` before it looks).
const ringPollers: (() => void)[] = [];
let ringLoop = false;
function pollRings(): void {
  for (const p of ringPollers) p();
}
function startRingLoop(): void {
  if (ringLoop) return;
  ringLoop = true;
  const tick = () => {
    pollRings();
    requestAnimationFrame(tick);
  };
  requestAnimationFrame(tick);
}

/**
 * Tauri's `Channel` delivers strictly in index order, and it never gives up on a gap. So one lost
 * message wedges the stream until the webview reloads. Every later message gets buffered
 * (unbounded) behind the gap, silently. Ways that happens here:
 *  - a subscriber throws: Tauri advances its index only *after* `onmessage` returns;
 *  - a payload over 8 KiB (spectrum, scope) takes Tauri's async fetch path, and a failed fetch
 *    consumes its index with nothing but a `console.error` in the devtools;
 *  - the backend drops a subscriber whose `send` errored (`fan_out`).
 * All three look like "monitoring stopped, fixed by restarting the app", so each subscriber is
 * isolated and a stream that goes quiet while visible is re-registered on a fresh channel. The
 * backend's `register` replaces the old channel for this webview.
 *
 * With `ring`, the stream is read from its shared-memory ring instead where WebView2 can (above),
 * delivered to the same subscribers once a frame; the `Channel` is the fallback.
 */
function makeStream<T>(cmd: string, ring?: { kind: RingKind; decode: (time: number, arrays: Float32Array[]) => T }) {
  const subs = new Set<(v: T) => void>();
  let channel: Channel<T> | null = null;
  let mode: "none" | "pending" | "channel" | "ring" = "none";
  let lastMsg = 0;
  let lastRegister = 0;
  let stalled = false; // warn once per quiet spell, not on every retry

  const deliver = (v: T) =>
    subs.forEach((f) => {
      try {
        f(v);
      } catch (e) {
        console.error(`[streams] ${cmd} subscriber threw`, e);
      }
    });

  function register() {
    // Drop the old channel's callback, and any messages parked behind its gap.
    (channel as unknown as { cleanupCallback?: () => void } | null)?.cleanupCallback?.();
    const ch = new Channel<T>();
    ch.onmessage = (v) => {
      if (ch !== channel) return;
      lastMsg = performance.now();
      stalled = false;
      deliver(v);
    };
    channel = ch;
    lastRegister = performance.now();
    void invoke(cmd, { channel: ch });
  }

  function watchdog() {
    const now = performance.now();
    if (mode !== "channel" || subs.size === 0 || document.hidden) return;
    if (now - visibleSince < STALL_MS || now - lastRegister < STALL_MS) return;
    if (now - lastMsg < STALL_MS) return;
    if (!stalled) console.warn(`[streams] ${cmd}: no data for ${Math.round((now - lastMsg) / 1000)} s, re-registering`);
    stalled = true;
    register();
  }

  function start() {
    setInterval(watchdog, 1000);
    startHeartbeat();
    if (!ring) {
      mode = "channel";
      return register();
    }
    const { kind, decode } = ring;
    requestRing(
      kind,
      () => {
        mode = "ring";
        (channel as unknown as { cleanupCallback?: () => void } | null)?.cleanupCallback?.();
        channel = null;
        ringPollers.push(() => {
          const r = rings[kind];
          if (r && subs.size > 0) forEachNew(r, (at) => {
            const { time, arrays } = readRecord(r, at);
            deliver(decode(time, arrays));
          });
          else if (r) r.read = r.u32[4]; // nobody listening: skip, don't queue up
        });
        startRingLoop();
      },
      () => {
        if (mode === "ring") return;
        mode = "channel";
        register();
      },
    );
  }

  return {
    /** Start receiving this stream; returns the unsubscribe function. */
    subscribe(fn: (v: T) => void): () => void {
      subs.add(fn);
      if (mode === "none") {
        mode = "pending"; // until the ring arrives or the fallback starts
        start();
      }
      return () => subs.delete(fn);
    },
  };
}

export const meterStream = makeStream<MeterData>("subscribe_meter", { kind: "meter", decode: decodeMeter });
export const spectrumStream = makeStream<SpectrumData>("subscribe_spectrum", { kind: "spectrum", decode: decodeSpectrum });
export const scopeStream = makeStream<ScopeData>("subscribe_scope");

/** This webview's scope playout clock (scopeClock.ts), shared by its scope views: fed from the
 *  shared ring (or the scope stream) while any view holds a reader. */
const scopeClock = new ScopeClock();
let clockUsers = 0;
let clockUnsub: (() => void) | null = null;

// Spectrum updates on the same playout timeline: each one applied once the playhead reaches its
// analysis window's end (`t_end`), so they land evenly — one per FFT step, 42.7 ms apart — and in
// step with playback and the scopes, however unevenly they arrive. Without a running clock (no
// scope view in this window yet), or for an update without a time (idle decay), as they arrive.
const SPEC_QUEUE_MAX = 8; // about a third of a second of updates
const specQueue: SpectrumData[] = [];
let specCur: SpectrumData | null = null;
let specNow = NaN;
let specSubscribed = false;

/** The spectrum update to show at frame time `nowMs` (see above); null before the first. Shared by
 *  this window's spectrum views, so they show the same one; the same `nowMs` gives the same one. */
export function spectrumAt(nowMs: number): SpectrumData | null {
  if (!specSubscribed) {
    specSubscribed = true;
    spectrumStream.subscribe((s) => {
      specQueue.push(s);
      if (specQueue.length > SPEC_QUEUE_MAX) specQueue.splice(0, specQueue.length - SPEC_QUEUE_MAX);
    });
  }
  if (nowMs === specNow) return specCur;
  specNow = nowMs;
  pollRings(); // whatever the ring has by now, not only what this frame's ring loop picked up
  const p = clockUsers > 0 ? scopeClock.playhead(nowMs) : NaN;
  while (specQueue.length > 0) {
    const s = specQueue[0];
    if (!Number.isNaN(p) && s.t_end > 0 && s.t_end / 1e4 > p) break;
    specCur = s;
    specQueue.shift();
  }
  return specCur;
}

// The scope stream from its shared ring (above), read by the clock itself once a frame (its
// source), or over the `Channel` if the ring doesn't come.
let scopeFeed: "none" | "pending" | "ring" | "channel" = "none";

/** Push the scope windows written since the last look into the clock. */
function pollScopeRing(): void {
  const r = rings.scope;
  if (!r) return;
  const now = performance.now();
  forEachNew(r, (at) => {
    const w = at >>> 2;
    const n = r.u32[w + 1];
    scopeClock.push(
      {
        xy: r.f32.subarray(w + 8, w + 8 + 2 * n),
        signal: (r.u32[w + 3] & 1) === 1,
        rate: r.u32[w + 2],
        t0: r.f64[(at + 16) >>> 3],
        sent: r.f64[(at + 24) >>> 3],
      },
      now,
      true,
    );
  });
}

/** Start feeding the clock: the shared ring if WebView2 hands one over, the `Channel` if not. */
function startScopeFeed(): void {
  if (scopeFeed === "ring") {
    const r = rings.scope;
    if (r) r.read = r.u32[4]; // only what's written from now on
    return;
  }
  if (scopeFeed !== "none") return;
  scopeFeed = "pending";
  requestRing(
    "scope",
    () => {
      scopeFeed = "ring";
      scopeClock.setSource(pollScopeRing);
      clockUnsub?.(); // the ring took over after a fallback
      clockUnsub = null;
    },
    () => {
      if (scopeFeed === "ring") return;
      scopeFeed = "channel";
      clockUnsub = scopeStream.subscribe((s) => scopeClock.push(s, performance.now()));
    },
  );
}

/** A cursor on the scope stream's playout clock for one view; `release` when the view goes. */
export function scopeReader(): { reader: ScopeReader; release: () => void } {
  if (clockUsers++ === 0) startScopeFeed();
  let released = false;
  return {
    reader: scopeClock.reader(),
    release: () => {
      if (released) return;
      released = true;
      // The ring stays (the backend writes it regardless, and reading stops with the frames); a
      // `Channel` fallback is unsubscribed and taken again by the next view.
      if (--clockUsers === 0 && clockUnsub) {
        clockUnsub();
        clockUnsub = null;
        scopeFeed = "none";
      }
    },
  };
}
