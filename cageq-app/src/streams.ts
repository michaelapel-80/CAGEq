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
 * for the webview's whole lifetime — the backend's subscriber list drops it when the webview
 * dies (send failure). Components subscribe/unsubscribe *locally* (a Set of callbacks, no IPC),
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
      }
      return () => subs.delete(fn);
    },
  };
}

export const meterStream = makeStream<MeterData>("subscribe_meter");
export const spectrumStream = makeStream<SpectrumData>("subscribe_spectrum");
export const scopeStream = makeStream<ScopeData>("subscribe_scope");
