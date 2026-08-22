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
 * for the webview's whole lifetime — the backend keys its subscriber list by webview label and
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

function makeStream<T>(cmd: string) {
  const subs = new Set<(v: T) => void>();
  let registered = false;
  return {
    /** Start receiving this stream; returns the unsubscribe function. */
    subscribe(fn: (v: T) => void): () => void {
      subs.add(fn);
      if (!registered) {
        registered = true;
        const channel = new Channel<T>();
        channel.onmessage = (v) => subs.forEach((f) => f(v));
        void invoke(cmd, { channel });
        startHeartbeat();
      }
      return () => subs.delete(fn);
    },
  };
}

export const meterStream = makeStream<MeterData>("subscribe_meter");
export const spectrumStream = makeStream<SpectrumData>("subscribe_spectrum");
export const scopeStream = makeStream<ScopeData>("subscribe_scope");
