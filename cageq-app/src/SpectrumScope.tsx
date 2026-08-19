import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { listen, emit } from "@tauri-apps/api/event";
import { composedCurveDb } from "./biquad";
import { fcHue } from "./fcColor";
import { spectrumStream } from "./streams";
import { createPhosphor } from "./phosphor";
import type { SpectrumData } from "./EqChart";
import type { ScopeEq } from "./Vectorscope";

/** Live-tunable render parameters — same rationale as Vectorscope/TimeScope's panels. `trailTau`
 *  is a genuine phosphor decay time constant, and `tail` the multiplier applied to it for faint
 *  content — both handed to the shared half-float accumulator (phosphor.ts), exactly as the scope
 *  views do. `undistort` shares the scope family's meaning and machinery (same
 *  `scope-eq` broadcast, same "filters + preamp" correction EqChart's own spectrum backdrop now
 *  applies) — see `getCorrection` below for why it's a frequency-domain subtraction here rather
 *  than the scopes' sample-domain inverse cascade: this view never sees raw samples, only the
 *  backend's already-FFT'd, already-log-binned dB values. */
type Params = { trailTau: number; tail: number; glow: number; undistort: boolean };
const DEFAULTS: Params = { trailTau: 0.15, tail: 12, glow: 0.15, undistort: true };

/** Cache for the per-bin correction curve (filter response + preamp, dB), keyed by reference/value
 *  so it's rebuilt only when the EQ or bin layout actually changes, not on every 60 fps frame. */
type CorrCache = { filters: ScopeEq["filters"] | null; preampDb: number; n: number; arr: Float64Array };
function getCorrection(cache: { current: CorrCache | null }, eq: ScopeEq, s: SpectrumData): Float64Array {
  const c = cache.current;
  if (c && c.filters === eq.filters && c.preampDb === eq.preampDb && c.n === s.db.length) return c.arr;
  const n = s.db.length;
  const arr = new Float64Array(n);
  if (eq.filters.length) {
    const lnF0 = Math.log(s.f_min);
    const lnF1 = Math.log(s.f_max);
    const bf = new Float64Array(n);
    for (let i = 0; i < n; i++) bf[i] = Math.exp(lnF0 + (i / (n - 1)) * (lnF1 - lnF0));
    const curve = composedCurveDb(eq.filters, bf);
    for (let i = 0; i < n; i++) arr[i] = curve[i] + eq.preampDb;
  } else {
    arr.fill(eq.preampDb); // Dry: no filters, still undo the §4.1 loudness-match preamp
  }
  cache.current = { filters: eq.filters, preampDb: eq.preampDb, n, arr };
  return arr;
}
const REF_SIZE = 512;
const GRID_ALPHA = 0.22;
// Same fixed dBFS scale as EqChart's spectrum backdrop (§5.4) — consistent reading between the
// Frequency pane's backdrop and this standalone analyzer.
const SPEC_TOP_DB = 0;
const SPEC_DYN = 90;
const F_MIN = 20;
const F_MAX = 20000;
const FREQ_TICKS = [100, 1000, 10000]; // unlabeled-chart-clutter-avoiding minimum: one per decade

/** The tangent at point `i` for a *monotone* non-uniform cubic Hermite spline (Fritsch-Carlson in
 *  spirit: constrain the tangent so the curve can't leave the range its neighbours bound it to).
 *  Each secant weighted by the *opposite* segment's width, so a short adjacent segment pulls the
 *  tangent toward its own slope instead of the far one's — matters here because the backend's
 *  per-bin Gaussian reduction (`cageq-monitor`'s `gaussian_power`) doesn't put bin centres at even
 *  Hz spacing on this log axis in general. Endpoints just use the one available secant.
 *
 *  A spline was tried here before, over three live-tested rounds, and thrown out (`tracePolyline`,
 *  a plain polyline, replaced it — see git history) once it became clear the actual bug wasn't the
 *  curve shape at all: the backend's OLD `max`-based reduction let many adjacent display bins share
 *  the *exact* same value (oversampling a coarse linear FFT grid), and the dedup built to collapse
 *  those literal ties kept discarding or reinventing shape across gaps in a way a spline's tangent
 *  math couldn't cleanly recover from. `gaussian_power` replaced that reduction entirely — a smooth
 *  function of each bin's own (never-repeating) fractional range, so adjacent bins essentially never
 *  produce bit-for-bit identical values any more. With no ties, there's nothing to dedup: every bin
 *  gets its own spline control point, `findPeaks`' reported bin is *always* one of them by
 *  construction (a Hermite spline passes exactly through every point it's given, regardless of
 *  tangent rule), and the monotone tangent here exists only for what's left — a plain sharp
 *  residual bump (the backend fix's own case history notes one, ~2.7 dB, 41 dB down — see
 *  `gaussian_power`'s doc) can't make the curve swing past its own true height on the way through. */
function hermiteTangent(xs: Float64Array, ys: Float64Array, n: number, i: number): number {
  if (i <= 0) return (ys[1] - ys[0]) / (xs[1] - xs[0]);
  if (i >= n - 1) return (ys[n - 1] - ys[n - 2]) / (xs[n - 1] - xs[n - 2]);
  const hL = xs[i] - xs[i - 1];
  const hR = xs[i + 1] - xs[i];
  const sL = (ys[i] - ys[i - 1]) / hL;
  const sR = (ys[i + 1] - ys[i]) / hR;
  if (sL === 0 || sR === 0 || sL > 0 !== sR > 0) return 0; // local extremum — flatten, don't overshoot it
  const avg = (hR * sL + hL * sR) / (hL + hR);
  const cap = Math.min(Math.abs(sL), Math.abs(sR));
  return Math.sign(avg) * Math.min(Math.abs(avg), cap);
}

/** Stroke a smooth cubic Hermite spline through `n` points `(xs[i], ys[i])`, tangents from
 *  `hermiteTangent` — passes exactly through every point, correct for arbitrarily-spaced x.
 *  Converted to cubic Bezier per segment (tangent scaled by a third of the segment's own width —
 *  the standard Hermite-to-Bezier conversion, and *why* it needs the true per-segment width rather
 *  than assuming a uniform one) since canvas has no native spline primitive. Must already be inside
 *  a `beginPath()`; caller strokes. */
function traceSmooth(ctx: CanvasRenderingContext2D, xs: Float64Array, ys: Float64Array, n: number) {
  ctx.moveTo(xs[0], ys[0]);
  for (let i = 0; i < n - 1; i++) {
    const h = xs[i + 1] - xs[i];
    const m0 = hermiteTangent(xs, ys, n, i);
    const m1 = hermiteTangent(xs, ys, n, i + 1);
    const cp1x = xs[i] + h / 3;
    const cp1y = ys[i] + (m0 * h) / 3;
    const cp2x = xs[i + 1] - h / 3;
    const cp2y = ys[i + 1] - (m1 * h) / 3;
    ctx.bezierCurveTo(cp1x, cp1y, cp2x, cp2y, xs[i + 1], ys[i + 1]);
  }
}

/** Parse a `#rrggbb` hex (the `--accent` CSS var) to [r,g,b]; a green phosphor fallback. */
function parseHex(hex: string): [number, number, number] {
  const m = hex.trim().match(/^#?([0-9a-f]{6})$/i);
  if (!m) return [80, 220, 140];
  const n = parseInt(m[1], 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

/** The representative index for a tied run spanning `[i, j]` — its middle, rounded. Used by
 *  `findPeaks` to mark a plateau at its centre rather than its (arbitrary) leading edge. In
 *  practice `i === j` essentially always now — the backend's Gaussian reduction (see
 *  `hermiteTangent`'s doc) is a smooth function of each bin's own never-repeating fractional
 *  range, so exact ties between adjacent bins are no longer expected the way they were under the
 *  old `max`-based reduction. Kept rather than special-cased away: still correct, still cheap, and
 *  still needed if a tie ever *does* land exactly (two adjacent bins integrating to the identical
 *  float by coincidence isn't provably impossible, just no longer routine). */
function runMid(i: number, j: number): number {
  return Math.round((i + j) / 2);
}

/** Format a peak frequency for the numeric readout below the tube — unlike EqChart's `fmtHz`
 *  (built for a handful of fixed, always-round grid-tick values), this has to handle an arbitrary
 *  continuous bin frequency without printing a long float tail. */
function fmtPeakHz(hz: number): string {
  return hz >= 1000 ? `${(hz / 1000).toFixed(hz >= 10000 ? 1 : 2)} kHz` : `${Math.round(hz)} Hz`;
}

// Up to this many peaks are ever shown/marked — a handful more than one but still scannable at a
// glance without crowding the readout row or the tube itself.
const PEAK_COUNT = 5;
// #e6a23c — the same amber TimeScope's peak-hold lines use (see PEAK_LINE_ALPHA there), so a
// "peak" reads as the same colour wherever this app marks one.
const PEAK_MARK_COLOR = "rgba(230,162,60,0.9)";
// How far (dB) a local maximum must stand above the lower of the two valleys separating it from
// taller ground before it counts as a real peak — see `findPeaks`. Plain "> both neighbours" flags
// nearly every wiggle in FFT-noisy content; this rejects a shallow shoulder bump on a bigger peak's
// flank, which never finds a low-enough valley before running into that bigger peak.
const PEAK_MIN_PROMINENCE_DB = 6;
// Minimum spacing between picked peaks, in octaves (so it means the same thing at the low and high
// end of a log axis, unlike a fixed Hz or bin-count gap). ~a third-octave — roughly a critical
// band in the midrange — stops one broad resonance's own ripples from filling every slot.
const PEAK_MIN_SEPARATION_OCTAVES = 1 / 3;
// How far (dB) below the loudest content in the current frame a candidate may sit and still count
// as a real peak, not noise-floor texture. PEAK_MIN_PROMINENCE_DB alone isn't enough down at the
// noise floor: it only asks "is this bump taller than its immediate valleys", and a floor's natural
// statistical ripple routinely clears 6dB purely by chance somewhere across 240 bins — e.g. a clean
// 1kHz sine visibly showing a second "peak" at 5.77kHz, -103dB, ~80dB below the real tone. 60dB is
// a standard analyzer noise-floor gate: generous enough to keep real, quiet harmonics (a sawtooth's
// ladder is nowhere near 60dB down within the range anyone's looking at), tight enough to reject
// content that's actually down at the floor.
const PEAK_MAX_RANGE_DB = 60;

/** Parabolic (quadratic) interpolation across the three log bins straddling a peak at integer index
 *  `i`, refining both its reported frequency and level to sub-bin precision. Without this, a peak
 *  can only ever be reported at one of the 240 fixed log-bin centres — increasingly coarse in
 *  absolute Hz as frequency rises, since log bins are constant-*percentage* wide, not constant-Hz
 *  (~1.5% here: ~15Hz at 1kHz, ~150Hz at 10kHz). That's why a dead-on 1kHz sine could only ever
 *  read as its nearest bin centre, ~990Hz, however precisely the backend located it. Standard
 *  technique (the same used for sub-bin FFT peak/pitch estimation), adapted to operate on the
 *  already log-binned, Gaussian-smoothed display curve rather than raw FFT bins — reasonable since
 *  that curve is itself smooth and unimodal near a real, isolated tone, not the discontinuous
 *  step function the old `max`-based reduction produced (interpolating across that would have
 *  been meaningless). Returns `i` untouched at an array edge or a plateau (flat top — the parabola
 *  is undefined there, denominator ~0), both rare with the current reduction. */
function interpolatePeak(v: Float64Array, i: number, n: number): { i: number; v: number } {
  if (i <= 0 || i >= n - 1) return { i, v: v[i] };
  const ym1 = v[i - 1];
  const y0 = v[i];
  const yp1 = v[i + 1];
  const denom = ym1 - 2 * y0 + yp1;
  if (Math.abs(denom) < 1e-9) return { i, v: y0 };
  const d = Math.max(-0.5, Math.min(0.5, (0.5 * (ym1 - yp1)) / denom));
  return { i: i + d, v: y0 - 0.25 * (ym1 - yp1) * d };
}

/** Up to `PEAK_COUNT` distinct spectral peaks in `v[0..n)` (bin i's frequency given by `binHz`):
 *  local maxima prominent enough to be a real peak rather than FFT noise, loud enough to be real
 *  content rather than noise-floor ripple (`PEAK_MAX_RANGE_DB`), spaced far enough apart that they
 *  aren't all just one resonance's shoulder. Returns the *largest* qualifying peaks, then reorders
 *  them to ascending frequency — picking by magnitude and presenting by frequency are different
 *  steps on purpose, so a strong low-frequency hum and a quieter but still-qualifying high note
 *  both land in the order a reader scans the axis, not loudest-first. */
function findPeaks(v: Float64Array, n: number, binHz: (i: number) => number): { i: number; v: number }[] {
  if (n < 3) return [];
  // 1) Local maxima, plateau-aware. A run of bins tied at *exactly* the same value is common here
  // — the backend rounds dB to 1 decimal (see `SpectrumUpdate::db`), so the true rounded-off top of
  // an ordinary rounded peak often lands several adjacent bins wide, not one. An earlier version of
  // this scan flagged only the first (lowest-frequency) bin of such a run — the simple `v[i] >
  // v[i-1] && v[i] >= v[i+1]` test a plain per-bin scan uses necessarily does, since it has no
  // notion of "this whole flat stretch is one peak" — which put every marker at the run's leading
  // edge instead of its middle, visibly off the curve's drawn (and genuinely rounded) apex. Walking
  // each run's full extent and reporting its *centre* fixes that; for a true single-bin peak (no
  // tie) the run has length 1 and this reduces to exactly the old per-bin test.
  const candidates: { i: number; v: number }[] = [];
  for (let i = 1; i < n - 1; ) {
    if (v[i] <= v[i - 1]) {
      i++;
      continue;
    }
    let j = i;
    while (j + 1 < n && v[j + 1] === v[i]) j++; // extend across the tied plateau
    if (j + 1 < n && v[j + 1] < v[i]) candidates.push({ i: runMid(i, j), v: v[i] });
    i = j + 1; // either past a confirmed peak, or past a run that turned out to keep rising/hit the edge
  }
  // 2) Prominence: walk outward from each candidate until the ground rises back above it (or the
  // array ends), tracking the lowest point crossed each way. A shoulder bump never finds a valley
  // deep enough before running into the bigger peak it's riding on; a standalone peak does.
  const prominent = candidates.filter((c) => {
    let leftMin = c.v;
    for (let i = c.i - 1; i >= 0 && v[i] <= c.v; i--) leftMin = Math.min(leftMin, v[i]);
    let rightMin = c.v;
    for (let i = c.i + 1; i < n && v[i] <= c.v; i++) rightMin = Math.min(rightMin, v[i]);
    return c.v - Math.max(leftMin, rightMin) >= PEAK_MIN_PROMINENCE_DB;
  });
  // 2.5) Noise-floor gate: prominence alone can't tell a real quiet feature from the floor's own
  // statistical ripple (see PEAK_MAX_RANGE_DB's doc) — this can, since it's relative to the loudest
  // thing actually in the frame rather than each candidate's own immediate neighbours.
  let loudest = -Infinity;
  for (let i = 0; i < n; i++) if (v[i] > loudest) loudest = v[i];
  const audible = prominent.filter((c) => loudest - c.v <= PEAK_MAX_RANGE_DB);
  // 3) Greedy pick by magnitude, skipping anything too close (in octaves) to an already-picked
  // peak — otherwise the loudest region's own harmonics could fill every remaining slot.
  audible.sort((a, b) => b.v - a.v);
  const picked: { i: number; v: number }[] = [];
  for (const c of audible) {
    if (picked.length >= PEAK_COUNT) break;
    const f = binHz(c.i);
    if (picked.some((p) => Math.abs(Math.log2(f / binHz(p.i))) < PEAK_MIN_SEPARATION_OCTAVES)) continue;
    picked.push(c);
  }
  // 4) Presented by frequency, not the magnitude order they were picked in. Interpolated last,
  // after every index-based comparison above (prominence's neighbour walk, the octave-separation
  // check) is done with the coarse integer bin — those decisions don't need sub-bin precision, only
  // the final reported frequency/level do.
  picked.sort((a, b) => a.i - b.i);
  return picked.map((p) => interpolatePeak(v, p.i, n));
}

/**
 * §5.4 CRT-styled spectrum analyzer — the "Monitor" chart view's own instrument, replacing the
 * earlier approach of reusing `EqChart` with every curve/marker/node stripped. That worked but
 * looked like an EQ chart with nothing on it; this is a dedicated analyzer sharing the
 * vectorscope/time-scope's visual language (dark `.vs-screen`, cached gradients, the same
 * `.vs-tools`/`.vs-tuning` chrome, and the same phosphor-persistence *look* — though after that
 * look's stored-image implementations repeatedly misbehaved on this rendering stack, the trail
 * here is owned by the shared accumulator in phosphor.ts, as the scopes' are). The Frequency pane
 * (`EqChart`, curves + its own spectrum backdrop) is untouched — this only replaces Monitor.
 *
 * A connected spline through the backend's log-frequency bins (§5.4 `SpectrumUpdate`), not filled
 * bars — an earlier bar-graph version read as flat/clean rather than CRT-like; the additively
 * accumulating trail is what gives the CRT feel. Briefly a plain polyline instead (see
 * `hermiteTangent`'s doc) while the backend's reduction had a real bug a spline's tangent math
 * couldn't cleanly represent — restored now that bug is fixed at its source, since interpolating
 * every bin exactly is strictly better once there's nothing left for a curve to get wrong. Each
 * rendered frame draws the
 * latest received event's values as-is, with no temporal blending toward the previous one (also
 * tried; also worth losing — see the trail effect's doc) — EqChart's spectrum backdrop, fed the
 * identical data, has always drawn it this same unblended way. No separate
 * peak-hold marker — the trail shows "where this has recently been" on its own, so a second
 * indicator doing the same job was redundant. That was argued before it was true: the plain
 * exponential trail it was removed in favour of actually faded too fast to read as a peak hold.
 * `tail` (phosphor.ts) is what closed the gap — faint content now lingers long enough that the
 * afterglow genuinely *is* the resonance catcher the marker used to be. Mono — the FFT is computed
 * from the mono-summed loopback, there's no L/R spectrum to split.
 *
 * Owns its own `spectrum` subscription (no `set_scope_viewer`-style gating needed: unlike the
 * heavier `scope` stream, `spectrum` is always emitted whenever monitoring runs — Meter and
 * EqChart already consume it the same passive way).
 */
export function SpectrumScope() {
  const { t } = useTranslation();
  // Ref'd on `.vs-screen` (the CRT box itself), not the outer wrap — the wrap also hosts the
  // peak readout row below the tube (see the return JSX), and the canvases' backing-store
  // resolution must match the tube's own box exactly, not the taller box that includes the
  // readout, or the two drift and the render blurs.
  const screenRef = useRef<HTMLDivElement>(null);
  const gridRef = useRef<HTMLCanvasElement>(null);
  const trailRef = useRef<HTMLCanvasElement>(null);
  // Peak crosses live on their own cleared-every-frame layer, same reason as Vectorscope's resting
  // spot: the trail composites additively, so a constant-brightness redraw drawn straight into it
  // would stack toward white instead of holding steady (see phosphor.ts).
  const markRef = useRef<HTMLCanvasElement>(null);
  // Text for up to PEAK_COUNT readout chips, written imperatively (see the trail effect) — driving
  // this off React state from a 60 fps stream once re-rendered the entire App tree per arrival.
  // Split into two fixed-width fields (frequency, level) per chip, each written independently —
  // see `.ss-peak-hz`/`.ss-peak-db` in App.css for why: a single free-width text node reflows (and
  // visibly shifts every OTHER chip alongside it) whenever a peak's digit count changes, e.g.
  // "60 Hz" growing to "8.2 kHz" as a note bends upward.
  const peakSlotRefs = useRef<(HTMLSpanElement | null)[]>([]);
  const peakHzRefs = useRef<(HTMLSpanElement | null)[]>([]);
  const peakDbRefs = useRef<(HTMLSpanElement | null)[]>([]);
  // The latest received spectrum event — the trail effect below draws it straight, no temporal
  // interpolation toward a previous one (see the component doc comment for why: this used to blend
  // per-bin between the last two events over the gap between them, so the line would flow at 60 fps
  // despite events landing slower than that — but blending two snapshots of the same *frequency*
  // bin can shift what's really a small, honest step in *when* the spectrum changed into a false
  // wobble in *what* it reads, worst exactly on a steep transition, where two adjacent events'
  // values differ the most. EqChart's identical-data spectrum backdrop never showed this because it
  // draws one event at a time with no blending — the tell that pointed at the interpolation itself
  // rather than the data feeding it.
  const curRef = useRef<SpectrumData | null>(null);
  const eqRef = useRef<ScopeEq>({ filters: [], preampDb: 0 });
  const corrCacheRef = useRef<CorrCache | null>(null);
  const [params, setParams] = useState<Params>(DEFAULTS);
  const [tuning, setTuning] = useState(false);
  const paramsRef = useRef(params);
  paramsRef.current = params;

  // Fills the chart-wrap (not square, unlike the vectorscope; not sharing a row, unlike the time
  // scope), minus a small strip at the bottom for the peak readout — both dimensions tracked from
  // `.vs-screen`'s own box (App.css gives it `flex: 1 1 auto` inside the wrap, so it's exactly
  // "chart-wrap's height, less the readout row"), so the canvases' resolution always matches what
  // CSS actually renders instead of drifting from a hardcoded or stale value.
  const [width, setWidth] = useState(320);
  const [height, setHeight] = useState(215);
  useEffect(() => {
    const el = screenRef.current;
    if (!el) return;
    const measure = () => {
      const r = el.getBoundingClientRect();
      setWidth(Math.max(80, Math.floor(r.width)));
      setHeight(Math.max(60, Math.floor(r.height)));
    };
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    measure();
    return () => ro.disconnect();
  }, []);
  const dpr = Math.min(typeof window !== "undefined" ? window.devicePixelRatio || 1 : 1, 2);
  const resW = Math.round(width * dpr);
  const resH = Math.round(height * dpr);

  // Passive subscriber — Meter.tsx owns starting/stopping the underlying capture; the stream is
  // the Channel-backed bus (streams.ts, not `listen` events), same as EqChart's backdrop. Also
  // picks up the `scope-eq` broadcast for undistort — same event Vectorscope/TimeScope consume,
  // requested on mount since events aren't retained.
  useEffect(() => {
    const unsubSpectrum = spectrumStream.subscribe((s) => {
      curRef.current = s;
    });
    let active = true;
    let unlistenEq: (() => void) | undefined;
    void (async () => {
      unlistenEq = await listen<ScopeEq>("scope-eq", (e) => {
        if (active) eqRef.current = e.payload;
      });
      if (active) void emit("scope-eq-request");
    })();
    return () => {
      active = false;
      unsubSpectrum();
      unlistenEq?.();
    };
  }, []);

  // Static graticule: a few dBFS reference lines + one vertical guide per frequency decade
  // (100/1k/10k — deliberately minimal, this view's whole point is staying uncluttered). Redrawn
  // only on resize.
  useEffect(() => {
    const cv = gridRef.current;
    const ctx = cv?.getContext("2d");
    if (!cv || !ctx) return;
    const W = cv.width;
    const H = cv.height;
    ctx.clearRect(0, 0, W, H);
    const [ar, ag, ab] = parseHex(getComputedStyle(cv).getPropertyValue("--accent"));
    const plotTop = H * 0.03;
    const plotBot = H * 0.97;

    ctx.strokeStyle = `rgba(${ar},${ag},${ab},${GRID_ALPHA})`;
    ctx.lineWidth = Math.max(1, H / REF_SIZE);
    ctx.beginPath();
    for (const db of [0, -20, -40, -60, -80]) {
      const y = plotBot - ((db - (SPEC_TOP_DB - SPEC_DYN)) / SPEC_DYN) * (plotBot - plotTop);
      ctx.moveTo(0, y);
      ctx.lineTo(W, y);
    }
    const lnMin = Math.log(F_MIN);
    const lnSpan = Math.log(F_MAX) - lnMin;
    for (const hz of FREQ_TICKS) {
      const x = ((Math.log(hz) - lnMin) / lnSpan) * W;
      ctx.moveTo(x, plotTop);
      ctx.lineTo(x, plotBot);
    }
    ctx.stroke();

    ctx.fillStyle = `rgba(${ar},${ag},${ab},0.5)`;
    ctx.font = `${Math.round(H * 0.045)}px system-ui, sans-serif`;
    ctx.textAlign = "center";
    ctx.textBaseline = "bottom";
    for (const hz of FREQ_TICKS) {
      const x = ((Math.log(hz) - lnMin) / lnSpan) * W;
      ctx.fillText(hz >= 1000 ? `${hz / 1000}k` : `${hz}`, x, H - 2);
    }
  }, [resW, resH]);

  // The main trace + its phosphor trail. This frame's line is drawn into a scratch 2D canvas and
  // handed to the shared half-float accumulator (phosphor.ts), which owns the decay and the
  // additive composite — the same machinery both scopes use.
  //
  // It used to redraw the whole trail from a timestamped stamp ring every frame instead, because at
  // the time no in-place decay could be made to reach zero: an 8-bit `destination-out` fade stalls
  // wherever alpha drops below ~0.5/(1-keep) LSB, which at this instrument's slow trails is a
  // permanent ~7% ghost. That ring worked, but it was a workaround for a substrate limit, and a
  // costly one — every live stamp re-splined every frame. A spike later found the limit wasn't
  // inherent: the earlier WebGL attempt that seemed to confirm it had been writing into
  // `UNSIGNED_BYTE` textures, so its float shader math was rounded straight back into the same
  // 8-bit trap. Half-float storage removes the stall at its source, so the trail can simply
  // accumulate again — no ring, no cutoff, no per-frame history redraw.
  //
  // The one thing that must survive the change: the beam is still gated on `signal`. The backend
  // sweeps `db` down to the floor during silence, and *drawing* that sweep repaints the whole
  // region under the last curve into the trail — which was the original burn-in complaint, and is
  // a separate cause from the 8-bit stall (see `SpectrumUpdate.signal`).
  useEffect(() => {
    const cv = trailRef.current;
    const markCv = markRef.current;
    const markCtx = markCv?.getContext("2d");
    if (!cv || !markCv || !markCtx) return;
    const phos = createPhosphor(cv);
    if (!phos) return;
    const [ar, ag, ab] = parseHex(getComputedStyle(cv).getPropertyValue("--accent"));

    let raf = 0;
    let last = performance.now();
    // The stroke gradient depends only on theme + plot geometry + glow — cached and rebuilt only
    // when the key actually changes, instead of every frame (see EqChart's identical
    // `specGradCache` fix — a fresh CanvasGradient costs the GPU compositor a shader/texture
    // upload each time, a real contributor to the GPU-memory growth diagnosed earlier this session).
    let gradKey = "";
    let grad: CanvasGradient | null = null;
    // The peak readout's DOM writes, throttled independently of the (unthrottled) crosses below —
    // the underlying data is already smoothed at the source (cageq-monitor's SPEC_TAU_SECS), but
    // text updating 60x/s reads as vibrating rather than as a number, in a way a moving cross
    // doesn't. `hadPeak` blanks the row exactly once on losing signal / peaks, rather than writing
    // to it every idle frame for nothing.
    let lastReadout = 0;
    let hadPeak = false;
    const READOUT_INTERVAL_MS = 120;
    // Reused per-point scratch buffers for the trace (see `traceSmooth`) — resized, never
    // reallocated fresh each frame, matching TimeScope's `magScratch` pattern.
    let xScratch = new Float64Array(0);
    let yScratch = new Float64Array(0);
    // Every bin's (possibly corrected) value, one-to-one with xScratch/yScratch — `findPeaks` reads
    // this directly for true bin-to-bin adjacency (needed to detect local maxima correctly).
    let vScratch = new Float64Array(0);

    const render = () => {
      const now = performance.now();
      const dt = Math.min(0.1, (now - last) / 1000); // clamp after a tab-switch stall
      last = now;
      const p = paramsRef.current;
      const W = cv.width;
      const H = cv.height;
      const plotTop = H * 0.03;
      const plotBot = H * 0.97;

      const ctx = phos.begin();
      // Additive, not source-over: source-over is a weighted blend *toward* the stroke colour, so
      // repeated strokes converge on fully-opaque accent blue and stop — it structurally cannot
      // exceed its own hue however many stack. "lighter" sums R/G/B independently with no hue
      // ceiling, so a genuine dwell keeps adding light until each channel clips, i.e. white. Set
      // every frame because a canvas resize resets the whole 2D context state.
      ctx.globalCompositeOperation = "lighter";

      const s = curRef.current;
      const n = s?.db.length ?? 0;
      if (n >= 2 && s && s.signal) {
        const corr = p.undistort ? getCorrection(corrCacheRef, eqRef.current, s) : null;
        const key = `${plotTop}|${plotBot}|${ar},${ag},${ab}|${p.glow}`;
        if (key !== gradKey) {
          gradKey = key;
          grad = ctx.createLinearGradient(0, plotBot, 0, plotTop);
          grad.addColorStop(0, `rgba(${ar},${ag},${ab},${p.glow * 0.35})`);
          grad.addColorStop(1, `rgba(${ar},${ag},${ab},${p.glow})`);
        }
        ctx.strokeStyle = grad!;
        ctx.lineWidth = Math.max(1, H / REF_SIZE) * 2;
        ctx.lineJoin = "round";
        ctx.lineCap = "round";

        if (xScratch.length < n) {
          xScratch = new Float64Array(n);
          yScratch = new Float64Array(n);
          vScratch = new Float64Array(n);
        }
        // One point per bin, no deduplication — see `hermiteTangent`'s doc for why that's safe now:
        // the backend's Gaussian reduction essentially never produces two adjacent bins with the
        // exact same value the way the old `max`-based one routinely did, so there's no "tied run"
        // left to collapse or preserve the shape of.
        for (let i = 0; i < n; i++) {
          vScratch[i] = corr ? s.db[i] - corr[i] : s.db[i];
          const frac = Math.max(0, Math.min(1, (vScratch[i] - (SPEC_TOP_DB - SPEC_DYN)) / SPEC_DYN));
          xScratch[i] = (i / (n - 1)) * W;
          yScratch[i] = plotBot - frac * (plotBot - plotTop);
        }
        // n >= 2 already guaranteed by the outer `if`.
        ctx.beginPath();
        traceSmooth(ctx, xScratch, yScratch, n);
        ctx.stroke();

        // Peak crosses: recomputed and redrawn every frame (not throttled — see below), so they
        // track the live trace exactly as fluidly as the trace itself does.
        markCtx.clearRect(0, 0, W, H);
        const lnF0 = Math.log(s.f_min);
        const lnSpan = Math.log(s.f_max) - lnF0;
        const binHz = (i: number) => Math.exp(lnF0 + (i / (n - 1)) * lnSpan);
        const peaks = findPeaks(vScratch, n, binHz);
        if (peaks.length) {
          markCtx.strokeStyle = PEAK_MARK_COLOR;
          markCtx.lineWidth = Math.max(1, H / REF_SIZE) * 1.5;
          const r = Math.max(3, H * 0.018); // cross arm length
          markCtx.beginPath();
          for (const pk of peaks) {
            const frac = Math.max(0, Math.min(1, (pk.v - (SPEC_TOP_DB - SPEC_DYN)) / SPEC_DYN));
            const x = (pk.i / (n - 1)) * W;
            const y = plotBot - frac * (plotBot - plotTop);
            markCtx.moveTo(x - r, y);
            markCtx.lineTo(x + r, y);
            markCtx.moveTo(x, y - r);
            markCtx.lineTo(x, y + r);
          }
          markCtx.stroke();
        }

        // The readout row's text, throttled independently of the (unthrottled) crosses above — see
        // `lastReadout`'s own comment.
        if (now - lastReadout > READOUT_INTERVAL_MS) {
          lastReadout = now;
          hadPeak = peaks.length > 0;
          for (let j = 0; j < PEAK_COUNT; j++) {
            const slot = peakSlotRefs.current[j];
            const hzSpan = peakHzRefs.current[j];
            const dbSpan = peakDbRefs.current[j];
            if (!slot || !hzSpan || !dbSpan) continue;
            if (j < peaks.length) {
              const hz = binHz(peaks[j].i);
              hzSpan.textContent = fmtPeakHz(hz);
              dbSpan.textContent = `${peaks[j].v.toFixed(1)} dB`;
              // Same Fc→hue mapping ToneGrid's Fc readout uses, at the same full strength — a
              // peak's frequency reads as the same colour here as a band tuned to it would there.
              slot.style.color = fcHue(hz);
            } else {
              hzSpan.textContent = "";
              dbSpan.textContent = "";
            }
          }
        }
      } else if (hadPeak) {
        // Signal just dropped — blank once rather than leaving the last reading stale on screen
        // (matching the beam itself, which the `signal` gate above also stops updating on silence).
        hadPeak = false;
        markCtx.clearRect(0, 0, W, H);
        for (const hzSpan of peakHzRefs.current) if (hzSpan) hzSpan.textContent = "";
        for (const dbSpan of peakDbRefs.current) if (dbSpan) dbSpan.textContent = "";
      }

      phos.commit(dt, p.trailTau, p.tail);
      raf = requestAnimationFrame(render);
    };
    raf = requestAnimationFrame(render);
    return () => {
      cancelAnimationFrame(raf);
      phos.dispose();
    };
  }, []);

  const set = <K extends keyof Params>(k: K, v: Params[K]) => setParams((prev) => ({ ...prev, [k]: v }));
  type NumKey = "trailTau" | "tail" | "glow";
  const CONTROLS: { key: NumKey; label: string; min: number; max: number; step: number }[] = [
    { key: "trailTau", label: t("scope.trail"), min: 0.02, max: 0.6, step: 0.01 },
    { key: "tail", label: t("scope.tail"), min: 1, max: 64, step: 1 },
    { key: "glow", label: t("scope.glow"), min: 0.05, max: 1, step: 0.05 },
  ];

  return (
    <div className="spectrumscope-wrap">
      {/* width/height here are CSS "100%" (matching this box exactly, sub-pixel precise, since its
          own size comes from the flex rule in App.css rather than an inline style) — the
          JS-measured `resW`/`resH` state feeds only the canvas backing-store *resolution*
          attributes below, never the display size. A JS-measured, floored pixel value here would
          drift by up to 1px from the box's true (CSS-computed) height and show up as exactly the
          kind of small persistent misalignment against EqChart's own height:auto SVG sizing. */}
      <div className="vs-screen" ref={screenRef}>
        <canvas
          ref={gridRef}
          className="vectorscope-canvas vs-grid"
          width={resW}
          height={resH}
          style={{ width: "100%", height: "100%" }}
          aria-hidden="true"
        />
        <canvas
          ref={trailRef}
          className="vectorscope-canvas vs-trail"
          width={resW}
          height={resH}
          style={{ width: "100%", height: "100%" }}
          aria-hidden="true"
        />
        <canvas
          ref={markRef}
          className="vectorscope-canvas vs-peakmarks"
          width={resW}
          height={resH}
          style={{ width: "100%", height: "100%" }}
          aria-hidden="true"
        />
        <div className="vs-tools">
          <button
            type="button"
            className={`vs-tool${tuning ? " on" : ""}`}
            title={t("scope.tune")}
            aria-pressed={tuning}
            onClick={() => setTuning((v) => !v)}
          >
            ⚙
          </button>
        </div>
        {tuning && (
          <div className="vs-tuning">
            <div className="vs-tune-head">
              <span className="vs-tune-title">{t("scope.tune")}</span>
              <button type="button" className="vs-tune-reset" onClick={() => setParams(DEFAULTS)}>
                {t("scope.reset")}
              </button>
              <button type="button" className="vs-tune-close" title={t("scope.close")} aria-label={t("scope.close")} onClick={() => setTuning(false)}>
                ×
              </button>
            </div>
            {CONTROLS.map((cc) => (
              <label key={cc.key} className="vs-tune-row">
                <span className="vs-tune-label">{cc.label}</span>
                <input
                  type="range"
                  min={cc.min}
                  max={cc.max}
                  step={cc.step}
                  value={params[cc.key]}
                  onChange={(e) => set(cc.key, Number(e.currentTarget.value))}
                />
                <b>{params[cc.key].toFixed(cc.step >= 1 ? 0 : cc.step >= 0.1 ? 1 : 2)}</b>
              </label>
            ))}
            <div className="vs-tune-sep" />
            <label className="vs-tune-row vs-tune-check" title={t("scope.undistortHint")}>
              <span className="vs-tune-label">{t("scope.undistort")}</span>
              <input type="checkbox" checked={params.undistort} onChange={(e) => set("undistort", e.currentTarget.checked)} />
            </label>
          </div>
        )}
      </div>
      {/* Numeric peak readout, below the tube — up to PEAK_COUNT chips, one per cross marked on the
          tube above, presented left-to-right by frequency (see `findPeaks`). The trail's own long
          afterglow (`tail`, phosphor.ts) already shows *where* the spectrum has recently been, but
          reading an exact level or frequency off a glowing curve isn't realistic — this is the same
          information as a number. A fixed PEAK_COUNT of slots is rendered upfront, ALWAYS all
          PEAK_COUNT of them (see App.css — no more collapsing an empty slot to `display:none`), so
          the row's own width and each chip's own position never depend on how many peaks are
          currently found; and each chip is two independently-sized fixed-width fields (frequency,
          level — App.css again) rather than one free-width text node, so a peak sliding from
          "60 Hz" to "8.2 kHz" can't shift every chip after it sideways either. Slots are
          shown/blanked by writing their text (see the trail effect) rather than mapping over a
          variable-length array, since the array itself lives outside React state — see below.
          Updated imperatively, NOT via React state: `spectrum` arrives well above React's
          comfortable render rate, and driving a state update from it once re-rendered the entire
          App tree per arrival (see cageq-monitor's `SpectrumUpdate` doc / this component's own
          history) — the fix there was moving the data off state entirely, so adding a state-driven
          readout here would reintroduce exactly that. */}
      <div className="ss-readout" title={t("scope.peak")}>
        {Array.from({ length: PEAK_COUNT }, (_, j) => (
          <span
            key={j}
            ref={(el) => {
              peakSlotRefs.current[j] = el;
            }}
            className="ss-peak"
          >
            <span
              ref={(el) => {
                peakHzRefs.current[j] = el;
              }}
              className="ss-peak-hz"
            />
            <span
              ref={(el) => {
                peakDbRefs.current[j] = el;
              }}
              className="ss-peak-db"
            />
          </span>
        ))}
      </div>
    </div>
  );
}
