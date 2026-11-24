// The scope stream's playout clock: each frame of a scope view shows the audio being played at
// that moment, so every frame advances by exactly its own frame time of audio, whatever the
// display's refresh rate and however the windows arrive.
//
// Why: the stream arrives in 10 ms windows at 100 a second, unevenly (measured: up to ~8 ms of
// spread in delivery, a rare ~30 ms stall), and the display runs at 60, 144 or 240 Hz. Drawn as
// they arrived, some frames got a window, some none, some two: content moved in steps that beat
// against the refresh rate.
//
// The clock: each window carries its first sample's time on the endpoint's own timeline —
// WASAPI's packet timestamp, which for loopback is when the samples are due at the device
// (measured 10–20 ms *after* the window is sent: the stream runs ahead of playback) — and when it
// was sent, both on the performance counter. `performance.now()` runs on the same counter, so
// the two differ by a constant: estimated as the least (arrival − sent) seen, which is that
// constant plus the fastest delivery (~a millisecond; the playhead lags playback by that much).
// The playhead is then "now" on the device's timeline, plus an advance A: A = 0 shows exactly what
// is being played; it leads by up to LEAD_FRAMES where the data allows (a frame reaches the screen
// a refresh or two after it is drawn), and goes below 0 only as far as delivery needs — the lowest
// headroom (how far the data reached past "now" as each window came) over the last minute — and
// no further than MAX_DELAY_MS. Left out of that: windows held up by the page itself stopping
// (STALL_FRAMES), by a held delivery like a window drag (later than the cap, and the burst queued
// behind it), the rare window far later than usual (OUTLIER_MS), and a run's start-up (counted
// only briefly). Those freeze a frame or a few and catch up instead. It
// moves slowly (SLEW_EARLIER/LATER), so playback speed never visibly changes, except at a run's
// start (START_DELAY_MS), where nothing is showing yet.
//
// Measured against synthetic streams with the live timing (60/144/240 Hz, stalls, drags,
// start-up, silence): each frame's samples follow its own frame time to the injected frame jitter,
// no sample lost or doubled. Frame times themselves come from frameClock.ts.
//
// Self-contained (no imports) so the synthetic harness can run it under Node as is.

/** The fields of a scope window this needs (cageq-monitor's `ScopeUpdate`). */
export type ScopeWindowIn = { xy: ArrayLike<number>; signal: boolean; rate: number; t0: number; sent: number };

/** A run of contiguous samples, `l`/`r` from `start`, `n` long. `contiguous`: it continues the
 *  samples this reader returned last, with nothing missing between them. */
export type ScopeSlice = { l: Float32Array; r: Float32Array; start: number; n: number; rate: number; contiguous: boolean };

/** One frame's audio. `state`: "data" — the playhead is within the data (`slices` may still be
 *  empty for a frame shorter than a sample); "starved" — the data stops short of it, but more is
 *  on its way (a late window); "idle" — the stream has nothing to show (silence, stopped). */
export type ScopeTake = { slices: ScopeSlice[]; state: "data" | "starved" | "idle"; rate: number };

type Win = { seq: number; l: Float32Array; r: Float32Array; n: number; rate: number; t0: number; end: number; contiguous: boolean };

/** Margin under the measured headroom, ms. */
const SAFETY_MS = 2;
/** The advance covers the lowest headroom over this long, ms: every late window but the rare
 *  outlier (OUTLIER_MS), not a quantile — the 5 lowest in 1000 left out starved a frame every
 *  couple of seconds on the measured delivery, each a hitch. A window that came late because the
 *  page itself had stopped (see STALL_FRAMES) doesn't count, and one from a run's first
 *  RUN_START_MS counts only for START_SPAN_MS. */
const HEADROOM_SPAN_MS = 60_000;
/** A window this much later than usual (its headroom this far under the median), ms, is a rare
 *  outlier: it freezes a frame or a few and catches up instead of setting the latency. Measured:
 *  one window ~26 ms late every few minutes (a delay in the backend's own wake-up, every frame on
 *  time) otherwise held the playhead ~28 ms behind playback for the minute after it — a lag you
 *  could see — against a typical low only ~7 ms under the median. */
const OUTLIER_MS = 10;
/** …as long as they stay rare: more than this many in the last HEADROOM_SPAN_MS, and they are how
 *  delivery is on this machine — covered again, rather than a hitch every few seconds. */
const OUTLIERS_MAX = 6;
/** Headroom samples the median is taken over (10 s of windows), and how often it's refreshed. */
const MEDIAN_SPAN = 1000;
const MEDIAN_EVERY = 100;
/** The playhead may lead playback by up to this many frames (at most LEAD_MAX_MS) where the
 *  headroom allows: a frame reaches the screen a refresh or two after it is drawn, so a little
 *  lead shows the picture nearer the sound. In practice the headroom allows only a millisecond or
 *  two. */
const LEAD_FRAMES = 1.5;
const LEAD_MAX_MS = 12;
/** Before this many samples, a little more margin under the lowest. */
const HEADROOM_MIN_SAMPLES = 50;
const HEADROOM_EARLY_MARGIN_MS = 3;
/** A gap between frames longer than this many frames (plus STALL_SLACK_MS) is the page having
 *  stopped — a long frame, a GC. A window sent before such a gap ended and arriving after it began
 *  was held up by it, not by slow delivery: the frames stopped with it, so it is no reason for more
 *  latency. Measured: one ~110 ms stall queued a dozen windows and, counted, took the advance to
 *  −145 ms for the minute after. The browser runs the next frame before the queued windows, so
 *  "long since the last frame" at arrival doesn't see it: the gap has to be remembered. */
const STALL_FRAMES = 2;
const STALL_SLACK_MS = 5;
/** Frame gaps remembered for that. */
const STALL_GAPS_KEPT = 4;
/** Delivery is worse while a stream starts (measured: a −30 ms low in the first second, against
 *  ~4 ms after). A window from a run's first RUN_START_MS lowers the advance as any does, but only
 *  for START_SPAN_MS, so start-up doesn't set the latency for the minute after. */
const RUN_START_MS = 1000;
const START_SPAN_MS = 2000;
/** The most the playhead lags playback, ms. A window later than this starves a frame or several
 *  (the trace holds, then catches up) instead of setting the latency: measured, Windows' modal
 *  move/resize loop holds the app's message delivery up by as much as ~135 ms while the page goes
 *  on drawing — covering that took the latency to −137 ms for the minute after a drag. Headroom
 *  below it doesn't count either, so a drag doesn't leave the latency at the cap. */
const MAX_DELAY_MS = 30;
/** Where a run starts (the stream was idle, nothing is on screen), ms behind playback: set at once,
 *  then relaxing toward what the headroom allows at SLEW_LATER (about half a second). Started
 *  optimistic instead, a run starved frames while start-up's worse delivery pulled it down: a
 *  choppy start; at MAX_DELAY_MS and 1%, the first 3 s lagged visibly. */
const START_DELAY_MS = 15;
/** A window arriving within this of the one before, ms, came queued behind it: if that one was
 *  left out (a stall, an outlier, or later than MAX_DELAY_MS), so is this — the burst a held delivery lets go
 *  is one event, and its last windows would otherwise set the latency just inside the cap. */
const BURST_MS = 1;
/** The fastest the playhead's offset moves, as a fraction of real time: earlier (more delay) by
 *  up to 5% when delivery got worse, later by up to 3% when it improved — playback runs that much
 *  slow or fast while it settles, which motion can't show. */
const SLEW_EARLIER = 0.05;
const SLEW_LATER = 0.03;
/** How far the clock offset estimate creeps up per window (ms), so a reading that was too low
 *  (a glitch) can't pin it forever: 0.1 ms a second. */
const BASE_CREEP_MS = 0.001;
/** A window starts a new run when its first sample is this far off the previous one's end, ms. */
const CONTIGUOUS_TOL_MS = 2;
/** Past the data's end by more than this, the stream counts as idle rather than late, ms. */
const IDLE_GRACE_MS = 100;
/** Windows kept, ms behind the playhead. */
const KEEP_MS = 1000;
/** A reader further behind the playhead than this (a view that stopped drawing) skips ahead, ms. */
const MAX_BEHIND_MS = 100;

export class ScopeClock {
  private wins: Win[] = [];
  private seq = 0;
  private base = Infinity; // least (arrival − sent), ms: performance.now() minus the counter, plus the fastest delivery
  private targetCache = NaN;
  private targetDirty = true;
  // The headroom's sliding minimum, over HEADROOM_SPAN_MS (and START_SPAN_MS for a run's start).
  private low = new SlidingMin(HEADROOM_SPAN_MS);
  private startLow = new SlidingMin(START_SPAN_MS);
  private samples = 0;
  private frameIv = NaN; // the frames' mean interval, ms (see STALL_FRAMES)
  private gaps: { start: number; end: number }[] = []; // recent frame gaps (see STALL_FRAMES)
  private runStart = -Infinity; // when the current run's first window arrived
  private lastArrival = -Infinity;
  private lastLeftOut = false; // whether the last window's headroom was left out (see BURST_MS)
  private recent = new Float64Array(MEDIAN_SPAN); // headroom samples, for the median (OUTLIER_MS)
  private recentN = 0;
  private recentI = 0;
  private median = NaN;
  private outliers: number[] = []; // when each outlier came (see OUTLIERS_MAX)
  private offset = NaN; // playhead (counter ms) − performance.now(), slewed toward its target
  private lastNow = NaN;
  private idle = true;
  // Sample buffers of dropped windows, reused for new ones: typed-array backing stores count as
  // external memory in V8, and allocating two per window (200 a second) is the kind of churn that
  // triggers full mark-compact GCs. A reader only holds a window's arrays within one frame, and
  // windows are only dropped as one arrives, between frames.
  private pool: Float32Array[] = [];
  // Where windows come from when they're read rather than delivered (shared memory, streams.ts):
  // called once per frame, before the playhead, to push what's new.
  private source: (() => void) | null = null;
  private sourceNow = NaN;

  /** Read windows from `fn` once per frame, as the playhead is asked for (see `source`). */
  setSource(fn: (() => void) | null): void {
    this.source = fn;
  }

  /** Add a window, as it arrives (`arrivalMs`: performance.now()). `atSent`: the window was
   *  available from the moment it was sent — read from shared memory (streams.ts), not delivered —
   *  so its headroom is measured at its `sent` time (mapped onto performance.now()), not at when
   *  the page next looked; nor can a page stall have held it up. `arrivalMs` still sets the clock
   *  offset: the least (arrival − sent) is the offset plus the fastest look. */
  push(s: ScopeWindowIn, arrivalMs: number, atSent = false): void {
    if (s.sent > 0) {
      this.base = Math.min(this.base + BASE_CREEP_MS, arrivalMs - s.sent / 1e4);
    }
    const at = atSent && s.sent > 0 && Number.isFinite(this.base) ? s.sent / 1e4 + this.base : arrivalMs;
    const n = Math.floor(s.xy.length / 2);
    if (!s.signal || n < 1 || !(s.t0 > 0) || !(s.rate > 0) || !Number.isFinite(this.base)) {
      if (!s.signal || n < 1) this.idle = true;
      return;
    }
    this.idle = false;
    const t0 = s.t0 / 1e4;
    const end = t0 + (n / s.rate) * 1000;
    const prev = this.wins[this.wins.length - 1];
    const contiguous = prev !== undefined && prev.rate === s.rate && Math.abs(t0 - prev.end) < CONTIGUOUS_TOL_MS;
    const nowQ = at - this.base;
    // Headroom: how far the data reached past "now" just before this window came — the low point
    // of the data's lead, the one the playhead has to stay under.
    if (!contiguous) {
      this.runStart = at;
      // A run after the stream went idle (or the first): nothing is showing, so the playhead can
      // move at once (see START_DELAY_MS).
      if (prev === undefined || nowQ - prev.end > IDLE_GRACE_MS) this.offset = -START_DELAY_MS - this.base;
    }
    // Held up by a page stall (see STALL_FRAMES): sent before a frame gap ended, arrived after it
    // began — or arriving now, long after the last frame.
    const sentPerf = s.sent / 1e4 + this.base;
    const stalled =
      !atSent &&
      (this.gaps.some((g) => sentPerf < g.end && at > g.start) ||
      (!Number.isNaN(this.lastNow) && !Number.isNaN(this.frameIv) && at - this.lastNow > STALL_FRAMES * this.frameIv + STALL_SLACK_MS));
    const h = contiguous ? prev.end - nowQ : NaN;
    const queued = this.lastLeftOut && at - this.lastArrival < BURST_MS;
    while (this.outliers.length > 0 && this.outliers[0] < at - HEADROOM_SPAN_MS) this.outliers.shift();
    const outlier = contiguous && !Number.isNaN(this.median) && h < this.median - OUTLIER_MS && this.outliers.length < OUTLIERS_MAX;
    if (outlier) this.outliers.push(at);
    if (contiguous && !stalled) this.noteRecent(h);
    const leftOut = stalled || queued || outlier || !(h > SAFETY_MS - MAX_DELAY_MS);
    this.lastArrival = at;
    this.lastLeftOut = contiguous && leftOut;
    if (contiguous && !leftOut) {
      (at - this.runStart < RUN_START_MS ? this.startLow : this.low).add(at, h);
      this.samples++;
    }
    this.low.expire(at);
    this.startLow.expire(at);
    this.targetDirty = true;
    const l = this.buffer(n);
    const r = this.buffer(n);
    for (let i = 0; i < n; i++) {
      l[i] = s.xy[2 * i];
      r[i] = s.xy[2 * i + 1];
    }
    this.wins.push({ seq: this.seq++, l, r, n, rate: s.rate, t0, end, contiguous });
    const keepFrom = nowQ - KEEP_MS;
    let drop = 0;
    while (drop < this.wins.length - 1 && this.wins[drop].end < keepFrom) drop++;
    if (drop > 0) {
      for (const w of this.wins.splice(0, drop)) this.pool.push(w.l, w.r);
      if (this.pool.length > 16) this.pool.length = 16;
    }
  }

  private buffer(n: number): Float32Array {
    const i = this.pool.findIndex((b) => b.length === n);
    return i < 0 ? new Float32Array(n) : this.pool.splice(i, 1)[0];
  }

  private noteRecent(h: number): void {
    this.recent[this.recentI] = h;
    this.recentI = (this.recentI + 1) % MEDIAN_SPAN;
    this.recentN = Math.min(MEDIAN_SPAN, this.recentN + 1);
    if (this.recentN >= HEADROOM_MIN_SAMPLES && (this.recentI % MEDIAN_EVERY === 0 || Number.isNaN(this.median))) {
      const sorted = this.recent.slice(0, this.recentN).sort();
      this.median = sorted[sorted.length >> 1];
    }
  }

  /** The advance the headroom allows (at most LEAD_FRAMES ahead), ms. */
  private target(): number {
    if (!this.targetDirty) return this.targetCache;
    this.targetDirty = false;
    const m = Math.min(this.low.min(), this.startLow.min());
    if (m === Infinity) return (this.targetCache = NaN);
    const low = m - (this.samples < HEADROOM_MIN_SAMPLES ? HEADROOM_EARLY_MARGIN_MS : 0);
    const lead = Number.isNaN(this.frameIv) ? 0 : Math.min(LEAD_MAX_MS, LEAD_FRAMES * this.frameIv);
    return (this.targetCache = Math.max(-MAX_DELAY_MS, Math.min(lead, low - SAFETY_MS)));
  }

  /** The playhead at `nowMs` (performance.now()): the device-timeline time this frame shows,
   *  counter ms. NaN before the first window. Idempotent for one `nowMs`. */
  playhead(nowMs: number): number {
    if (this.source && nowMs !== this.sourceNow) {
      this.sourceNow = nowMs;
      this.source();
    }
    if (Number.isNaN(this.offset)) return NaN;
    const a = this.target();
    if (!Number.isNaN(a) && !Number.isNaN(this.lastNow)) {
      const want = a - this.base;
      const span = Math.max(0, nowMs - this.lastNow);
      this.offset += Math.max(-SLEW_EARLIER * span, Math.min(SLEW_LATER * span, want - this.offset));
    }
    if (Number.isNaN(this.lastNow) || nowMs > this.lastNow) {
      const iv = nowMs - this.lastNow;
      if (!Number.isNaN(this.frameIv) && iv > STALL_FRAMES * this.frameIv + STALL_SLACK_MS) {
        this.gaps.push({ start: this.lastNow, end: nowMs });
        if (this.gaps.length > STALL_GAPS_KEPT) this.gaps.shift();
      } else if (iv > 0) {
        this.frameIv = Number.isNaN(this.frameIv) ? iv : this.frameIv + 0.05 * (iv - this.frameIv);
      }
      this.lastNow = nowMs;
    }
    return nowMs + this.offset;
  }

  /** How far the playhead is ahead of playback (≤ 0 — behind), ms; for diagnostics. */
  advanceMs(): number {
    return this.offset + this.base;
  }

  /** A cursor for one view: each `take` returns the audio since its last one. */
  reader(): ScopeReader {
    return new ScopeReader(this);
  }

  /** @internal */ get _wins(): readonly Win[] {
    return this.wins;
  }
  /** @internal */ get _idle(): boolean {
    return this.idle;
  }
}

/** The minimum of (time, value) samples over the last `span` ms: a deque with the values rising
 *  from the front, so the front is the minimum. */
class SlidingMin {
  private t: number[] = [];
  private v: number[] = [];
  private span: number;
  constructor(span: number) {
    this.span = span;
  }
  add(t: number, v: number): void {
    while (this.v.length > 0 && this.v[this.v.length - 1] >= v) {
      this.v.pop();
      this.t.pop();
    }
    this.t.push(t);
    this.v.push(v);
  }
  expire(now: number): void {
    while (this.t.length > 0 && this.t[0] < now - this.span) {
      this.t.shift();
      this.v.shift();
    }
  }
  min(): number {
    return this.v.length > 0 ? this.v[0] : Infinity;
  }
}

export class ScopeReader {
  private seq = -1; // the window the cursor is in, and the next sample in it
  private k = 0;
  private started = false;
  // A break in the samples not yet reported: set at a discontinuity (or the start), cleared by the
  // next slice — which carries it. While set the playhead is in a gap (before the next run starts).
  private broken = true;
  private lastRate = 48000;
  private clock: ScopeClock;
  constructor(clock: ScopeClock) {
    this.clock = clock;
  }

  /** The audio from the last take up to the playhead at `nowMs`. The slices reference the
   *  clock's own buffers: read them before the next window arrives (this frame), don't keep them. */
  take(nowMs: number): ScopeTake {
    const wins = this.clock._wins;
    const p = this.clock.playhead(nowMs);
    const slices: ScopeSlice[] = [];
    if (Number.isNaN(p) || wins.length === 0) return { slices, state: "idle", rate: this.lastRate };
    let wi = 0;
    if (!this.started) {
      // Start at the playhead: nothing before it.
      this.started = true;
      this.broken = true;
      wi = this.seek(p);
      if (wi < 0) {
        const last = wins[wins.length - 1];
        this.seq = last.seq;
        this.k = last.n;
        return { slices, state: this.stateAt(p), rate: this.lastRate };
      }
    } else {
      wi = wins.findIndex((w) => w.seq >= this.seq);
      if (wi < 0) wi = wins.length; // past the newest: wait for more
      else if (wins[wi].seq !== this.seq) {
        this.broken = true; // the cursor's window was dropped
        this.k = 0;
      }
      // Too far behind (a view that stopped drawing): skip ahead rather than replay it all.
      if (wi < wins.length && this.timeAt(wins[wi], this.k) < p - MAX_BEHIND_MS) {
        const s = this.seek(p - MAX_BEHIND_MS);
        if (s >= 0) {
          wi = s;
          this.broken = true;
        }
      }
    }
    for (; wi < wins.length; wi++) {
      const w = wins[wi];
      if (w.seq !== this.seq) {
        // Entering the next window: carried on from the one before only if the stream did.
        // (Reaching it means the one before was used up.)
        if (this.seq >= 0 && !(w.contiguous && w.seq === this.seq + 1)) this.broken = true;
        this.seq = w.seq;
        this.k = 0;
      }
      const kEnd = Math.min(w.n, Math.max(this.k, Math.ceil(((p - w.t0) * w.rate) / 1000 - 1e-9)));
      if (kEnd > this.k) {
        slices.push({ l: w.l, r: w.r, start: this.k, n: kEnd - this.k, rate: w.rate, contiguous: !this.broken });
        this.broken = false;
        this.lastRate = w.rate;
        this.k = kEnd;
      }
      if (this.k < w.n) break; // the playhead is inside this window
    }
    return { slices, state: this.stateAt(p), rate: this.lastRate };
  }

  private stateAt(p: number): ScopeTake["state"] {
    const wins = this.clock._wins;
    const last = wins[wins.length - 1];
    if (last === undefined) return "idle";
    if (p <= last.end) return this.broken ? "idle" : "data";
    return !this.clock._idle && p - last.end < IDLE_GRACE_MS ? "starved" : "idle";
  }

  private timeAt(w: Win, k: number): number {
    return w.t0 + (k * 1000) / w.rate;
  }

  /** Put the cursor at the first sample at or after time `t`; the window's index, or -1 if none. */
  private seek(t: number): number {
    const wins = this.clock._wins;
    for (let i = 0; i < wins.length; i++) {
      const w = wins[i];
      if (w.end > t) {
        this.seq = w.seq;
        this.k = Math.max(0, Math.min(w.n, Math.ceil(((t - w.t0) * w.rate) / 1000 - 1e-9)));
        return i;
      }
    }
    return -1;
  }
}
