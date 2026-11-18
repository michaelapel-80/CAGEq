import { useSyncExternalStore } from "react";
import { useTranslation } from "react-i18next";

/**
 * An optional cap on how often the instruments draw — for seeing how they behave at another
 * refresh rate without switching the display's, and for anyone who'd rather spend less CPU/GPU
 * on them. 0 is no cap: every display refresh draws, as before.
 *
 * Every render loop already measures its own `dt` from the clock, so skipping frames needs
 * nothing else from them: a skipped frame doesn't touch `last`, and the next drawn one decays and
 * doses for the whole real interval. Data that arrives in between is handled as at a slower
 * display — the scopes trace the latest window, exactly as they would on one.
 *
 * Off a whole divisor of the display rate a cap can't be met exactly: 60 on a 144 Hz display
 * draws on every second or third refresh (60 on average). A real 60 Hz display's even cadence
 * needs the display itself, or a cap that divides it (72 or 48 on 144 Hz).
 *
 * Set from a row at the bottom of every instrument's tuning panel (`FrameCapRow`), one setting
 * for all of them. Persisted in localStorage, which the pop-out windows share: the `storage`
 * event carries a change made in one window to the others.
 */
const KEY = "cageq-frame-cap";

/** The slider's stops, slowest first; 0 (no cap) is the last one. */
export const FRAME_CAPS = [30, 48, 60, 72, 90, 120, 144, 0] as const;

function read(): number {
  try {
    const v = Number(localStorage.getItem(KEY));
    return Number.isFinite(v) && v > 0 ? v : 0;
  } catch {
    return 0;
  }
}

let cap = read();
const listeners = new Set<() => void>();
const notify = () => listeners.forEach((l) => l());

window.addEventListener("storage", (e) => {
  if (e.key !== KEY) return;
  cap = read();
  notify();
});

export function setFrameCap(fps: number) {
  cap = Number.isFinite(fps) && fps > 0 ? fps : 0;
  try {
    localStorage.setItem(KEY, String(cap));
  } catch {
    // Not persisted, still applied for this session.
  }
  notify();
}

export function useFrameCap(): number {
  return useSyncExternalStore(
    (l) => {
      listeners.add(l);
      return () => listeners.delete(l);
    },
    () => cap,
  );
}

/**
 * One per render loop: `due(now)` says whether this frame should draw under the current cap.
 *
 * Paced against a schedule advancing one interval per drawn frame, not against the last frame
 * drawn: "at least an interval since the last one" only ever lands on the first refresh *after*
 * the interval, so 60 on a 144 Hz display would draw every third refresh (48 Hz). The schedule
 * lets a frame up to 15% of an interval early (the loops read `performance.now()`, which jitters
 * by however late the callback ran) and still averages exactly the cap. Falling more than an
 * interval behind (a stall, a hidden window, a cap just lowered) restarts it from now rather than
 * drawing back-to-back to catch up.
 */
export function frameGate(): (now: number) => boolean {
  let next = 0;
  return (now) => {
    if (cap <= 0) {
      next = 0;
      return true;
    }
    const interval = 1000 / cap;
    if (now < next - 0.15 * interval) return false;
    next = now - next > interval ? now + interval : next + interval;
    return true;
  };
}

/** The tuning panels' frame-rate row: a slider over `FRAME_CAPS`. */
export function FrameCapRow() {
  const { t } = useTranslation();
  const cap = useFrameCap();
  const stop = (FRAME_CAPS as readonly number[]).indexOf(cap);
  const idx = stop >= 0 ? stop : FRAME_CAPS.length - 1; // only a hand-edited cap is off the stops
  return (
    <label className="vs-tune-row" title={t("fps.hint")}>
      <span className="vs-tune-label">{t("fps.label")}</span>
      <input
        name="frame-cap"
        type="range"
        min={0}
        max={FRAME_CAPS.length - 1}
        step={1}
        value={idx}
        onChange={(e) => setFrameCap(FRAME_CAPS[Number(e.currentTarget.value)])}
      />
      <b>{cap > 0 ? cap : t("fps.max")}</b>
    </label>
  );
}
