// The display's frame times, on the refresh grid. rAF's timestamp marks when a frame began, but
// WebView2 rounds it to 0.1 ms (performance.now()'s coarsening), so at 240 Hz the 4.167 ms period
// reads 4.1, 4.2 or 4.3 frame to frame: views that move with time moved 2–3% further on some frames
// than others, which a slow free-running scroll shows as a jitter. The refresh is a fixed grid, so
// this estimates its period and puts each frame on it: n periods after the last frame (n > 1 for a
// skipped frame, a capped rate, a stall), the phase pulled gently toward the timestamps. A frame
// off the grid by more than PHASE_TOL_MS (a different display, a long stall, a cap that never
// shows the refresh period itself) is taken as it is, and the grid starts again from it.
//
// Self-contained (no imports), so a harness can run it under Node.

/** How far off the predicted grid point a frame may land and still be put on it, ms: a few times
 *  the rounding (±0.05 ms). Fixed, not a fraction of the period: a grid learned wrong (a cap whose
 *  frames are all 2 and 3 refreshes apart) must fail it, not be held to a looser tolerance. */
const PHASE_TOL_MS = 0.25;
/** How much of the gap to the timestamp the phase closes per frame. */
const PHASE_GAIN = 0.05;
/** How much of a one-period interval's difference the period estimate takes per frame. */
const PERIOD_GAIN = 0.02;
/** Restarts in a row after which the period is taken afresh from the last interval. */
const RESEED_AFTER = 3;
/** Above this share of frames off the grid (a running mean), the timestamps are used as they are:
 *  a cap that lands on the refresh unevenly (60 on 144 Hz: 2 and 3 refreshes apart) has no grid
 *  of its own to find, and a grid that fits only some of its frames was measured worse than none. */
const MAX_MISS_RATE = 0.05;
const MISS_RATE_GAIN = 0.02;

export type FrameClock = { time(ts: number): number };

export function createFrameClock(): FrameClock {
  let raw = NaN; // the last timestamp
  let est = NaN; // its time on the grid
  let period = NaN;
  let misses = 0;
  let missRate = 0;
  return {
    /** `ts` (rAF's timestamp) on the refresh grid. The same `ts` gives the same answer. */
    time(ts: number): number {
      if (ts === raw) return est;
      if (Number.isNaN(raw) || !(ts > raw)) {
        raw = est = ts;
        return est;
      }
      const d = ts - raw;
      raw = ts;
      if (Number.isNaN(period) || d < period * 0.7) {
        // The first interval, or one shorter than the period: the refresh is faster than thought.
        period = d;
        est = ts;
        misses = 0;
        return est;
      }
      const n = Math.max(1, Math.round(d / period));
      const err = ts - (est + n * period);
      const miss = Math.abs(err) > PHASE_TOL_MS;
      missRate += MISS_RATE_GAIN * ((miss ? 1 : 0) - missRate);
      if (miss) {
        if (++misses >= RESEED_AFTER) {
          period = d;
          misses = 0;
        }
        est = ts;
        return est;
      }
      misses = 0;
      if (n === 1) period += PERIOD_GAIN * (d - period);
      est += n * period + PHASE_GAIN * err;
      return missRate > MAX_MISS_RATE ? ts : est;
    },
  };
}

/** This webview's frame clock, shared by its views (one display, one grid). */
export const frameClock = createFrameClock();
