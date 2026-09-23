import { invoke, Channel } from "@tauri-apps/api/core";
import type { SpectrumData } from "./EqChart";
import type { ScopeData } from "./Vectorscope";

/**
 * §5.3c data plane, frontend side. The meter/spectrum/scope streams used to arrive as Tauri
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
 * every stream at ~60 Hz whenever the monitor runs (silence included — `signal: false`), so a
 * multi-second gap while visible is never normal traffic.
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
