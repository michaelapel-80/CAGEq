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
 */
function makeStream<T>(cmd: string) {
  const subs = new Set<(v: T) => void>();
  let channel: Channel<T> | null = null;
  let lastMsg = 0;
  let lastRegister = 0;
  let stalled = false; // warn once per quiet spell, not on every retry

  function register() {
    // Drop the old channel's callback, and any messages parked behind its gap.
    (channel as unknown as { cleanupCallback?: () => void } | null)?.cleanupCallback?.();
    const ch = new Channel<T>();
    ch.onmessage = (v) => {
      if (ch !== channel) return;
      lastMsg = performance.now();
      stalled = false;
      subs.forEach((f) => {
        try {
          f(v);
        } catch (e) {
          console.error(`[streams] ${cmd} subscriber threw`, e);
        }
      });
    };
    channel = ch;
    lastRegister = performance.now();
    void invoke(cmd, { channel: ch });
  }

  function watchdog() {
    const now = performance.now();
    if (subs.size === 0 || document.hidden) return;
    if (now - visibleSince < STALL_MS || now - lastRegister < STALL_MS) return;
    if (now - lastMsg < STALL_MS) return;
    if (!stalled) console.warn(`[streams] ${cmd}: no data for ${Math.round((now - lastMsg) / 1000)} s, re-registering`);
    stalled = true;
    register();
  }

  return {
    /** Start receiving this stream; returns the unsubscribe function. */
    subscribe(fn: (v: T) => void): () => void {
      subs.add(fn);
      if (!channel) {
        register();
        setInterval(watchdog, 1000);
        startHeartbeat();
      }
      return () => subs.delete(fn);
    },
  };
}

export const meterStream = makeStream<MeterData>("subscribe_meter");
export const spectrumStream = makeStream<SpectrumData>("subscribe_spectrum");
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
  const p = clockUsers > 0 ? scopeClock.playhead(nowMs) : NaN;
  while (specQueue.length > 0) {
    const s = specQueue[0];
    if (!Number.isNaN(p) && s.t_end > 0 && s.t_end / 1e4 > p) break;
    specCur = s;
    specQueue.shift();
  }
  return specCur;
}

// The scope stream through shared memory, where WebView2 can (src-tauri/src/shared_scope.rs, which
// documents the ring): the capture thread writes every window into a ring the page reads each
// frame — no message per window, so none of the per-message script, JSON and garbage a `Channel`
// costs (at 100 windows a second per webview, with the pop-out open, the renderer's GC fell
// behind). Requested when the first scope view starts; the `Channel` (`scopeStream`) if the
// buffer doesn't come.
const RING_MAGIC = 0x42534743;
const RING_VERSION = 1;
const RING_HEADER_BYTES = 64;
/** Windows short of a full lap the page reads back at most (see shared_scope.rs). */
const RING_SLOT_MARGIN = 8;
/** How long after asking the page waits for the buffer before using the `Channel` instead, ms. */
const RING_WAIT_MS = 2000;
type Ring = { u32: Uint32Array; f32: Float32Array; f64: Float64Array; slots: number; slotBytes: number; read: number };
type SharedBufferEvent = { additionalData?: { kind?: string }; getBuffer(): ArrayBuffer };
type WebViewBridge = { addEventListener(type: "sharedbufferreceived", fn: (e: SharedBufferEvent) => void): void };
let ring: Ring | null = null;
let feedStarted = false;

/** Push the windows written since the last look into the clock (once per frame, via its source). */
function pollRing(): void {
  const r = ring;
  if (!r) return;
  const written = r.u32[4];
  let k = r.read;
  if (((written - k) >>> 0) > r.slots - RING_SLOT_MARGIN) k = (written - (r.slots - RING_SLOT_MARGIN)) >>> 0;
  const now = performance.now();
  for (; k !== written; k = (k + 1) >>> 0) {
    const at = RING_HEADER_BYTES + (k % r.slots) * r.slotBytes;
    const w = at >>> 2;
    if (r.u32[w] !== ((k + 1) >>> 0)) continue; // being rewritten: lapped
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
  }
  r.read = written;
}

/** Start feeding the clock: the shared ring if WebView2 hands one over, the `Channel` if not. */
function startScopeFeed(): void {
  if (ring) {
    ring.read = ring.u32[4]; // only what's written from now on
    return;
  }
  if (feedStarted) return;
  feedStarted = true;
  let fallenBack = false;
  const fallback = (why: string) => {
    if (ring || fallenBack) return;
    fallenBack = true;
    console.warn(`[streams] scope stream over the Channel: ${why}`);
    clockUnsub = scopeStream.subscribe((s) => scopeClock.push(s, performance.now()));
  };
  const bridge = (window as unknown as { chrome?: { webview?: WebViewBridge } }).chrome?.webview;
  if (!bridge) return fallback("no WebView2 bridge");
  bridge.addEventListener("sharedbufferreceived", (e) => {
    if (e.additionalData?.kind !== "scope") return;
    const buf = e.getBuffer();
    const u32 = new Uint32Array(buf);
    if (u32[0] !== RING_MAGIC || u32[1] !== RING_VERSION) return console.warn("[streams] unknown scope ring layout");
    ring = { u32, f32: new Float32Array(buf), f64: new Float64Array(buf), slots: u32[2], slotBytes: u32[3], read: u32[4] };
    scopeClock.setSource(pollRing);
    if (fallenBack) {
      clockUnsub?.(); // the ring took over after all
      clockUnsub = null;
    }
  });
  invoke("subscribe_scope_shared").then(
    () => setTimeout(() => fallback("no buffer arrived"), RING_WAIT_MS),
    (e) => fallback(String(e)),
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
        feedStarted = false;
      }
    },
  };
}
