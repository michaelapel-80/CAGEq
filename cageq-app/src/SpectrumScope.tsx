import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { listen, emit } from "@tauri-apps/api/event";
import { composedCurveDb } from "./biquad";
import { fcHue } from "./fcColor";
import { spectrumStream } from "./streams";
import { createPhosphor } from "./phosphor";
import { traceSmooth } from "./spline";
import { useTunableParams } from "./useTunableParams";
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
const DEFAULTS: Params = { trailTau: 0.18, tail: 12, glow: 0.225, undistort: true };
// Trail/Glow orthogonality: at steady state (a dose added every commit, decaying at
// `exp(-dt/trailTau)` between them), accumulated brightness is approximately
// `dose_per_second * trailTau` (see phosphor.ts's DOSE_REF_FPS doc for the same derivation, and
// Vectorscope.tsx's identical TAU_REF for the fuller reasoning/history) — so moving Trail longer
// measurably brightens a steady/dwelling signal even with Glow untouched. Unlike the scope views,
// this one already redraws every animation frame (no dedup-by-payload-identity gate), so it doesn't
// need their separate SCOPE_DOSE_RATIO correction — commit()'s own dt/DOSE_REF_DT already fully
// normalizes its redraw cadence on its own. Only the `TAU_REF/trailTau` term is needed here.
// Anchored at a real 100 ms: `glow` reads as "steady-state brightness units per 100 ms of
// persistence", the same physical meaning as the scope views' own Glow.
//
// First attempt anchored at a full second (TAU_REF=1) — see Vectorscope.tsx's identical doc for why
// that shrinks `glow` enough to risk 8-bit canvas quantisation banding on the gradient stroke's
// faint end. 100ms shrinks `glow` by only 0.15/0.1 = 1.5x here (barely at all), staying comfortably
// clear of that cliff while still being a fixed, non-arbitrary reference. DEFAULTS.glow below is
// `old_glow * old_trailTau / TAU_REF` (0.15 * 0.15 / 0.1 = 0.225), chosen so the shipped default
// renders identically to before the original decoupling change — only the number's meaning moved,
// twice now.
const TAU_REF = 0.1;

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
// Eq pane's backdrop and this standalone analyzer.
const SPEC_TOP_DB = 0;
const SPEC_DYN = 90;
const F_MIN = 20;
const F_MAX = 20000;
// The classic 1-2-5 sequence — same set EqChart's own `GRID_HZ` uses, so the two charts' grids
// read as the same axis rather than two different conventions.
const FREQ_TICKS = [20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000];

// hermiteTangent/traceSmooth moved to spline.ts — EqChart's spectrum backdrop grew the same
// per-bin-staircase-visible-as-jagged problem this trace solved and now shares the fix.

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
// Plain cool white, not the peak amber — the hover cursor is a *reading tool*, not a detected
// feature, so it deliberately doesn't compete visually with a genuine peak cross.
const CURSOR_LINE_COLOR = "rgba(230,240,255,0.55)";
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
// A candidate within this many partials of a lower, already-established peak still counts as
// belonging to that peak's harmonic series (see `harmonicOf`) — a mains hum's 50/100/150/200 Hz
// ladder or a sawtooth's n*f0 shouldn't compete for their own readout slots once the fundamental
// they ride on is already shown. No real instrument's audible partials go much past this.
const HARMONIC_MAX_N = 16;
// How far (in cents — 1200ths of an octave, the standard log-pitch unit) a candidate may drift from
// an exact integer multiple and still count as that harmonic, rather than an unrelated peak that
// happens to land nearby. Cents rather than a flat Hz or percent tolerance for the same reason
// PEAK_MIN_SEPARATION_OCTAVES is in octaves: it means the same thing at 100 Hz and 10 kHz. Under a
// quarter-tone (50 cents) — the backend's own bin spacing is ~26 cents (~1.5%, see `interpolatePeak`),
// so this is under two bins of slack either side of the ideal ratio.
const HARMONIC_TOLERANCE_CENTS = 45;
// Shown in an empty readout slot instead of leaving it blank — an empty chip popping in and out of
// existence every time the peak count changes (even just from frame-to-frame noise near a gate's
// threshold) reads as more of a glitch than a fixed-width dash sitting there quietly does.
const PEAK_PLACEHOLDER_HZ = "--- Hz";
const PEAK_PLACEHOLDER_DB = "--- dB";

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

/** Is `f` an integer multiple (2nd..`HARMONIC_MAX_N`th partial) of `root`, within
 *  `HARMONIC_TOLERANCE_CENTS`? Used by `findPeaks` to fold a peak into a lower one's harmonic
 *  series. Deliberately only tests the pairwise ratio between two *actually detected* peaks —
 *  it doesn't try to infer an absent fundamental from its partials (e.g. content with 200/300/400 Hz
 *  present but no energy at the true 100 Hz root: 300 and 400 fold into neither 200 nor each other,
 *  since 1.5x and 2x-of-a-different-root aren't integer ratios of what's actually there). That's a
 *  real limitation, not nothing — but recovering it needs real pitch estimation (autocorrelation or
 *  harmonic-product-spectrum over the whole partial set), a different and much larger feature than
 *  decluttering the readout of ladders whose root *is* present and already shown. */
function harmonicOf(f: number, root: number): boolean {
  if (f <= root) return false;
  const n = Math.round(f / root);
  if (n < 2 || n > HARMONIC_MAX_N) return false;
  const cents = 1200 * Math.log2(f / (n * root));
  return Math.abs(cents) < HARMONIC_TOLERANCE_CENTS;
}

/** Up to `PEAK_COUNT` distinct spectral peaks in `v[0..n)` (bin i's frequency given by `binHz`):
 *  none at all when the whole frame is at or below the noise floor, local maxima prominent enough
 *  to be a real peak rather than FFT noise, loud enough to be real content rather than noise-floor
 *  ripple (`PEAK_MAX_RANGE_DB`), spaced far enough apart that they aren't all just one resonance's
 *  shoulder, and — of the peaks left standing — not an integer-ratio harmonic of a lower one that's
 *  also present (`harmonicOf`): a fundamental's own ladder folds into it rather than each partial
 *  spending a slot competing on its own. Returns the *largest* qualifying peaks, then reorders them
 *  to ascending frequency —
 *  picking by magnitude and presenting by frequency are different steps on purpose, so a strong
 *  low-frequency hum and a quieter but still-qualifying high note both land in the order a reader
 *  scans the axis, not loudest-first. */
function findPeaks(v: Float64Array, n: number, binHz: (i: number) => number): { i: number; v: number }[] {
  if (n < 3) return [];
  let loudest = -Infinity;
  for (let i = 0; i < n; i++) if (v[i] > loudest) loudest = v[i];
  // 0) Absolute silence gate: nothing in this frame reaches even the visible plot's own floor, so
  // it's pure noise-floor content, not real signal — no matter how it's shaped, none of it should
  // be marked. PEAK_MAX_RANGE_DB below can't catch this on its own: it's relative to `loudest`, and
  // a frame that's ALL noise floor still has *a* loudest bin, with the rest of the floor's own
  // ripple routinely within 60dB of it. Matters because `signal` (the caller's own gate) doesn't
  // catch this either — WASAPI keeps delivering (all near-zero) frames as long as a stream is open,
  // so it stays true right through digital silence — reported live as peak markers/readout not
  // clearing when audio actually stopped.
  if (loudest < SPEC_TOP_DB - SPEC_DYN) return [];
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
  // thing actually in the frame (computed at the top, step 0) rather than each candidate's own
  // immediate neighbours.
  const audible = prominent.filter((c) => loudest - c.v <= PEAK_MAX_RANGE_DB);
  // 3) Harmonic folding: scanning low-to-high frequency, a candidate that's an integer multiple of
  // an already-established root (`harmonicOf`) is absorbed into that root's family instead of
  // becoming a root itself — so only the *lowest* member of each detected harmonic series ever
  // competes for a slot below, regardless of which partial happens to be loudest (a resonance or a
  // speaker's own response routinely makes some harmonic louder than the true fundamental, but the
  // fundamental is still the one worth reporting). Ascending order matters: it's what makes "lowest
  // surviving member" the thing each family collapses to, and it's why a later, higher partial can
  // fold into a root added just before it in this same pass (100 → 200 → 300 → 400 all settle on
  // root 100, tested against roots seen so far, not just the original candidate).
  const byFreqAsc = audible.slice().sort((a, b) => binHz(a.i) - binHz(b.i));
  const roots: { i: number; v: number }[] = [];
  for (const c of byFreqAsc) {
    const f = binHz(c.i);
    if (!roots.some((r) => harmonicOf(f, binHz(r.i)))) roots.push(c);
  }
  // 4) Greedy pick by magnitude, skipping anything too close (in octaves) to an already-picked
  // peak — otherwise one broad resonance's own ripples could fill every remaining slot.
  roots.sort((a, b) => b.v - a.v);
  const picked: { i: number; v: number }[] = [];
  for (const c of roots) {
    if (picked.length >= PEAK_COUNT) break;
    const f = binHz(c.i);
    if (picked.some((p) => Math.abs(Math.log2(f / binHz(p.i))) < PEAK_MIN_SEPARATION_OCTAVES)) continue;
    picked.push(c);
  }
  // 5) Presented by frequency, not the magnitude order they were picked in. Interpolated last,
  // after every index-based comparison above (prominence's neighbour walk, the octave-separation
  // check) is done with the coarse integer bin — those decisions don't need sub-bin precision, only
  // the final reported frequency/level do.
  picked.sort((a, b) => a.i - b.i);
  return picked.map((p) => interpolatePeak(v, p.i, n));
}

/**
 * §5.4 CRT-styled spectrum analyzer — the "Spectrum" chart view's own instrument (named "Monitor"
 * until it earned a dedicated identity of its own — see the naming note on `chartView`'s
 * declaration in App.tsx), replacing the earlier approach of reusing `EqChart` with every curve/
 * marker/node stripped. That worked but looked like an EQ chart with nothing on it; this is a
 * dedicated analyzer sharing the vectorscope/time-scope's visual language (dark `.vs-screen`,
 * cached gradients, the same
 * `.vs-tools`/`.vs-tuning` chrome, and the same phosphor-persistence *look* — though after that
 * look's stored-image implementations repeatedly misbehaved on this rendering stack, the trail
 * here is owned by the shared accumulator in phosphor.ts, as the scopes' are). The Eq pane
 * (`EqChart`, curves + its own spectrum backdrop) is untouched — this only replaces Spectrum.
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
  // Hover cursor: the fraction (0..1) of the tube's own width the mouse is over, or `null` when not
  // hovering. Set directly by the pointer handlers below (no DOM write there — see the render
  // effect's cursor step for why those live in the rAF loop instead), read once per frame.
  const hoverRef = useRef<number | null>(null);
  const cursorElRef = useRef<HTMLDivElement | null>(null);
  const cursorHzRef = useRef<HTMLSpanElement | null>(null);
  const cursorDbRef = useRef<HTMLSpanElement | null>(null);
  const { params, setParams, saveAsDefault, resetToFactory } = useTunableParams("cageq-spectrum-params", DEFAULTS);
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

  // Static graticule: a few dBFS reference lines + one vertical guide per FREQ_TICKS entry (the
  // classic 1-2-5 sequence — matches EqChart's own grid). Redrawn only on resize.
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
    ctx.textBaseline = "bottom";
    for (const hz of FREQ_TICKS) {
      const x = ((Math.log(hz) - lnMin) / lnSpan) * W;
      // FREQ_TICKS' own extremes (F_MIN/F_MAX) land exactly on the plot's edges — centring their
      // label there would run it half off-canvas, so those two anchor to the inside edge instead;
      // everything in between still centres on its gridline as before.
      ctx.textAlign = hz === F_MIN ? "left" : hz === F_MAX ? "right" : "center";
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
    // Reported live: this view specifically (not TimeScope/Vectorscope) reads much darker on some
    // machines. `precise` (see phosphor.ts) was built for exactly this — silently falling back from
    // the half-float GPU accumulator to the old 8-bit one on a GPU that can't render half-float, or
    // if the WebGL context request itself fails outright (e.g. a machine-dependent context-count
    // ceiling, with four views each opening their own). Logged once per mount so it's checkable via
    // DevTools on an affected machine without needing to reproduce it here first.
    if (!phos.precise) console.warn("[SpectrumScope] phosphor fell back to the 8-bit canvas accumulator (no half-float GPU support)");
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
    // Whether the cursor label was showing last frame — same one-shot-hide idea as `hadPeak`, so
    // leaving the tube doesn't need a per-frame DOM write to keep confirming it's still hidden.
    let cursorShown = false;
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
      // Marks layer (peak crosses + the hover cursor below) is a plain, non-accumulating 2D canvas
      // — cleared and fully redrawn every frame regardless of signal state, unlike the phosphor
      // trail above. That's new as of the cursor: crosses alone only ever needed this while a
      // signal was live (see the removed `hadPeak`-triggered one-shot clear this replaced), but the
      // cursor has to keep redrawing on an otherwise-idle frame too — the frequency axis stays
      // meaningful with nothing playing, and without a clear every frame, a cursor line that moves
      // while idle would leave every previous position stroked on top of the last, since a plain
      // 2D context has no decay of its own the way the trail canvas does.
      markCtx.clearRect(0, 0, W, H);
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
        // track the live trace exactly as fluidly as the trace itself does. (Layer already cleared
        // above, unconditionally.)
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
              slot.classList.remove("ss-peak-empty");
            } else {
              hzSpan.textContent = PEAK_PLACEHOLDER_HZ;
              dbSpan.textContent = PEAK_PLACEHOLDER_DB;
              slot.style.color = "";
              slot.classList.add("ss-peak-empty");
            }
          }
        }
      } else if (hadPeak) {
        // Signal just dropped — reset to the placeholder once rather than leaving the last reading
        // stale on screen (matching the beam itself, which the `signal` gate above also stops
        // updating on silence). Layer clear itself is unconditional now (above); this only resets
        // the DOM readout text.
        hadPeak = false;
        for (let j = 0; j < PEAK_COUNT; j++) {
          const slot = peakSlotRefs.current[j];
          const hzSpan = peakHzRefs.current[j];
          const dbSpan = peakDbRefs.current[j];
          if (hzSpan) hzSpan.textContent = PEAK_PLACEHOLDER_HZ;
          if (dbSpan) dbSpan.textContent = PEAK_PLACEHOLDER_DB;
          if (slot) {
            slot.style.color = "";
            slot.classList.add("ss-peak-empty");
          }
        }
      }

      // Hover cursor: independent of signal state (drawn on top of whatever the block above left on
      // the marks layer, which is why it lives after it) — the frequency axis is fixed and still
      // worth reading with nothing playing, e.g. lining a cursor up against a grid tick. Frequency
      // comes from the same fixed F_MIN/F_MAX log mapping the grid ticks use, not `s.f_min`/`f_max`
      // (equal in practice, see `SpectrumUpdate::f_min`'s doc, but `s` itself can be null here) — so
      // the cursor keeps reading correctly even before the first spectrum event ever arrives. The dB
      // reading is stricter: only shown when `vScratch` was actually rebuilt this same frame (i.e.
      // `n >= 2 && s.signal`, mirrored from the branch above), never a stale array from a prior frame.
      const hoverFrac = hoverRef.current;
      if (hoverFrac !== null) {
        cursorShown = true;
        const x = hoverFrac * W;
        markCtx.strokeStyle = CURSOR_LINE_COLOR;
        markCtx.lineWidth = Math.max(1, H / REF_SIZE);
        markCtx.beginPath();
        markCtx.moveTo(x, plotTop);
        markCtx.lineTo(x, plotBot);
        markCtx.stroke();

        const lnMin = Math.log(F_MIN);
        const lnSpan = Math.log(F_MAX) - lnMin;
        const hz = Math.exp(lnMin + hoverFrac * lnSpan);
        let dbText = PEAK_PLACEHOLDER_DB;
        if (n >= 2 && s && s.signal) {
          // Linear interpolation between the two bins straddling the cursor — reads the underlying
          // data directly rather than the cosmetically-smoothed spline drawn through it (`traceSmooth`),
          // which is the right choice for a readout: the spline's only job is to look good between
          // points, not to claim sub-bin structure the data itself doesn't have.
          const fi = hoverFrac * (n - 1);
          const i0 = Math.floor(fi);
          const i1 = Math.min(n - 1, i0 + 1);
          const t = fi - i0;
          dbText = `${(vScratch[i0] * (1 - t) + vScratch[i1] * t).toFixed(1)} dB`;
        }
        if (cursorHzRef.current) cursorHzRef.current.textContent = fmtPeakHz(hz);
        if (cursorDbRef.current) cursorDbRef.current.textContent = dbText;
        if (cursorElRef.current) {
          // A CSS percentage, not a pixel offset computed from the `width`/`height` React state:
          // this effect mounts once (`[]` deps below) and never re-runs on resize, so a state value
          // closed over here would go stale the first time the box's actual size changed. Percent
          // of the (always current) CSS box needs no such measurement at all.
          cursorElRef.current.style.left = `${hoverFrac * 100}%`;
          cursorElRef.current.style.opacity = "1";
        }
      } else if (cursorShown) {
        cursorShown = false;
        if (cursorElRef.current) cursorElRef.current.style.opacity = "0";
      }

      // TAU_REF/p.trailTau is the Trail/Glow orthogonality fix — see TAU_REF's own doc.
      phos.commit(dt, p.trailTau, p.tail, TAU_REF / p.trailTau);
      raf = requestAnimationFrame(render);
    };
    raf = requestAnimationFrame(render);
    return () => {
      cancelAnimationFrame(raf);
      phos.dispose();
    };
  }, []);

  // Hover cursor pointer handlers — write only the ref, never the DOM directly: the render effect's
  // rAF loop (above) is what actually draws the line and updates the label text/position, once per
  // frame, matching the throttled-DOM-write discipline the peak readout already uses (`lastReadout`)
  // rather than a raw mousemove rate, which can fire well above 60 Hz on a high-poll-rate mouse.
  const onScopeHover = (e: React.MouseEvent<HTMLDivElement>) => {
    const r = e.currentTarget.getBoundingClientRect();
    hoverRef.current = Math.max(0, Math.min(1, (e.clientX - r.left) / r.width));
  };
  const onScopeHoverEnd = () => {
    hoverRef.current = null;
  };

  const set = <K extends keyof Params>(k: K, v: Params[K]) => setParams((prev) => ({ ...prev, [k]: v }));
  type NumKey = "trailTau" | "tail" | "glow";
  const CONTROLS: { key: NumKey; label: string; min: number; max: number; step: number }[] = [
    { key: "trailTau", label: t("scope.trail"), min: 0.02, max: 0.6, step: 0.01 },
    { key: "tail", label: t("scope.tail"), min: 1, max: 64, step: 1 },
    { key: "glow", label: t("scope.glow"), min: 0.02, max: 1, step: 0.02 },
  ];

  return (
    <div className="spectrumscope-wrap">
      {/* width/height here are CSS "100%" (matching this box exactly, sub-pixel precise, since its
          own size comes from the flex rule in App.css rather than an inline style) — the
          JS-measured `resW`/`resH` state feeds only the canvas backing-store *resolution*
          attributes below, never the display size. A JS-measured, floored pixel value here would
          drift by up to 1px from the box's true (CSS-computed) height and show up as exactly the
          kind of small persistent misalignment against EqChart's own height:auto SVG sizing. */}
      <div className="vs-screen" ref={screenRef} onMouseMove={onScopeHover} onMouseLeave={onScopeHoverEnd}>
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
        {/* Hover-cursor readout: frequency (+ level, while a signal is live) under the mouse, for
            reading a peak's exact numbers off the tube directly rather than waiting for it to win a
            readout slot below — the peak picker only ever shows up to PEAK_COUNT peaks and, on busy
            program material, which ones qualify can change faster than the row is readable (that's
            the whole reason this exists: the fixed readout row is for "what's here right now",
            this is for "what's *that*, right there"). Positioned via `left` in % (never px) and
            `opacity`, both written imperatively from the render loop's own rAF cadence, matching
            every other DOM write in this component — see the effect's cursor step. `pointer-events:
            none` (App.css) so the label itself can never steal the hover it's reporting. */}
        <div className="ss-cursor" ref={cursorElRef} aria-hidden="true">
          <span ref={cursorHzRef} className="ss-cursor-hz" />
          <span ref={cursorDbRef} className="ss-cursor-db" />
        </div>
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
              <button type="button" className="vs-tune-reset" onClick={saveAsDefault}>
                {t("scope.saveDefault")}
              </button>
              <button type="button" className="vs-tune-reset" onClick={resetToFactory}>
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
                <b>{params[cc.key].toFixed(cc.step >= 1 ? 0 : cc.step >= 0.1 ? 1 : cc.step >= 0.01 ? 2 : 3)}</b>
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
          "60 Hz" to "8.2 kHz" can't shift every chip after it sideways either. An empty slot shows
          PEAK_PLACEHOLDER_HZ/DB ("--- Hz"/"--- dB") rather than going fully blank, for the same
          reason: a chip popping between text and nothing every time the peak count changes reads
          as a glitch, where a quiet dash sitting in a fixed-width field doesn't. Slots are filled
          or reset to the placeholder by writing their text (see the trail effect) rather than
          mapping over a variable-length array, since the array itself lives outside React state.
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
