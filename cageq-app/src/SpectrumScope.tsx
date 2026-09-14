import { useEffect, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import { listen, emit } from "@tauri-apps/api/event";
import { composedCurveDb, type FadingCurve, retargetFadingCurve, stepFadingCurve } from "./biquad";
import { fcHue } from "./fcColor";
import { spectrumStream } from "./streams";
import { createPhosphor } from "./phosphor";
import { traceSmooth } from "./spline";
import { useTunableParams } from "./useTunableParams";
import { SPEC_FFT_SIZES, fftSizeIndex, type SpectrumData } from "./EqChart";
import type { ScopeEq } from "./Vectorscope";

/** Live-tunable render parameters — same rationale as Vectorscope/TimeScope's panels. `trailTau`
 *  is a genuine phosphor decay time constant, and `tail` the multiplier applied to it for faint
 *  content — both handed to the shared half-float accumulator (phosphor.ts), exactly as the scope
 *  views do. `undistort` shares the scope family's meaning and machinery (same
 *  `scope-eq` broadcast, same "filters + preamp" correction EqChart's own spectrum backdrop now
 *  applies) — see `getCorrection` below for why it's a frequency-domain subtraction here rather
 *  than the scopes' sample-domain inverse cascade: this view never sees raw samples, only the
 *  backend's already-FFT'd, already-log-binned dB values. */
// bloom/haze: phosphor.ts's opt-in glow tiers (see Vectorscope.tsx's own Params doc, tried there
// first) — a log-frequency curve rather than a dwelling point/beam was the one content shape this
// hadn't been tried against yet; confirmed live to fit better than expected, enabled by default
// with its own tuned numbers rather than Vectorscope's/TimeScope's.
// `linear` picks SpectrumUpdate's linear-axis bins (`db_lin`/`peak_db_lin`, constant-Hz spacing)
// over the default log ones — pure per-viewer display choice, unlike `fftSize`/`harmonicFold`
// (App.tsx-owned, since those are enforced by the one shared backend monitor): the backend always
// computes both, so two open views can independently pick their own axis with no coordination
// needed at all. Off by default — log is still the right axis for reading a tonal correction
// against perception/octaves, which is what this view is used for most of the time; linear is the
// occasional, deliberate choice for reading a harmonic series as the evenly-*spaced* comb it
// actually is (mains hum, a motor, this app's own square/sawtooth/pulse test signals).
type Params = { trailTau: number; tail: number; glow: number; bloom: number; haze: number; undistort: boolean; linear: boolean };
const DEFAULTS: Params = { trailTau: 0.2, tail: 18, glow: 0.2, bloom: 0.8, haze: 0.8, undistort: true, linear: false };
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

function computeCorrectionCurve(eq: ScopeEq, s: SpectrumData, n: number, sampleRate: number | undefined, linear: boolean): Float64Array {
  const arr = new Float64Array(n);
  if (eq.filters.length) {
    const bf = new Float64Array(n);
    if (linear) {
      for (let i = 0; i < n; i++) bf[i] = s.f_min + (i / (n - 1)) * (s.f_max - s.f_min);
    } else {
      const lnF0 = Math.log(s.f_min);
      const lnF1 = Math.log(s.f_max);
      for (let i = 0; i < n; i++) bf[i] = Math.exp(lnF0 + (i / (n - 1)) * (lnF1 - lnF0));
    }
    const curve = composedCurveDb(eq.filters, bf, sampleRate);
    for (let i = 0; i < n; i++) arr[i] = curve[i] + eq.preampDb;
  } else {
    arr.fill(eq.preampDb); // Dry: no filters, still undo the §4.1 loudness-match preamp
  }
  return arr;
}

/** Cache for the per-bin correction curve (filter response + preamp, dB): rebuilt only when the
 *  EQ, bin layout, axis mode, or sample rate actually changes, not on every 60 fps frame, and
 *  retargeted through a `FadingCurve` (biquad.ts) rather than swapped outright — snapping it the
 *  instant the EQ changes used to manufacture a one-frame jump in the drawn trace that isn't in
 *  the real (crossfaded) audio at all, see `apo-switch-artifacts` memory, "NOT a bug". `sampleRate`
 *  isn't cosmetic either: a biquad's response depends on it via the bilinear transform, and this
 *  used to silently assume `biquad.ts`'s default 48 kHz regardless of the device's real rate.
 *  `linear` needs its own explicit tracking, not just `curve.to.length`: `N_LIN_BINS`/`N_LOG_BINS`
 *  happen to match today, so a bin-count check alone wouldn't notice the axis mode changing under
 *  an unchanged length — the two curves have different *values* at the same length. */
type CorrCache = { filters: ScopeEq["filters"] | null; preampDb: number; sampleRate: number | undefined; linear: boolean; curve: FadingCurve };
function getCorrection(cache: { current: CorrCache | null }, eq: ScopeEq, s: SpectrumData, sampleRate: number | undefined, dtMs: number, linear: boolean): Float64Array {
  const c = cache.current;
  const n = linear ? s.db_lin.length : s.db.length;
  if (!c || c.curve.to.length !== n || c.linear !== linear) {
    const to = computeCorrectionCurve(eq, s, n, sampleRate, linear);
    cache.current = { filters: eq.filters, preampDb: eq.preampDb, sampleRate, linear, curve: retargetFadingCurve(null, to) };
    return to;
  }
  if (c.filters !== eq.filters || c.preampDb !== eq.preampDb || c.sampleRate !== sampleRate) {
    c.filters = eq.filters;
    c.preampDb = eq.preampDb;
    c.sampleRate = sampleRate;
    c.curve = retargetFadingCurve(c.curve, computeCorrectionCurve(eq, s, n, sampleRate, linear));
  }
  return stepFadingCurve(c.curve, dtMs);
}
const REF_SIZE = 512;
const GRID_ALPHA = 0.22;
// Time constant a light exponential smoothing filter applies to the drawn trace's Y values before
// stroking — same mechanism as EqChart's own `STROKE_SMOOTH_TAU`, ported here as the fix for the
// window-drag stutter (see the render loop's own comment): the backend only emits a new spectrum
// payload at ~60Hz, and drawing the raw target straight meant this trace's geometry was
// bit-identical for however many rAF frames land between two backend payloads. Even at a steady
// 240Hz that 4:1 ratio was already visibly steppy at short Trail settings (confirmed live: a longer
// Trail didn't smooth it out either, since more persistence just blends more copies of the same
// step together rather than adding real in-between motion) — dragging then made it far worse, since
// Windows' native window-move loop samples/presents frames on its own cadence, not necessarily
// locked to that same 4:1 ratio, so the 60Hz steps land at irregular intervals relative to what's
// actually shown — visible judder on top of the steadier-state steppiness, with the rAF loop itself
// completely unaffected either way (confirmed live: an on-canvas FPS counter never dropped through
// the stutter). Smoothing toward the target every frame instead means the drawn geometry is never
// twice identical, fixing both. Shorter than EqChart's own 0.05: this view's trace moves faster/more
// abruptly (raw FFT bins, no phase content to slow it down the way EqChart's filter-response curve
// has), and 0.05 read as visibly laggy here — 0.03 is barely above the backend's own SPEC_TAU_SECS
// (0.02) smoothing, just enough to fix the stepping without adding a perceptible extra delay.
const STROKE_SMOOTH_TAU = 0.03;
// Same fixed dBFS scale as EqChart's spectrum backdrop (§5.4) — consistent reading between the
// Eq pane's backdrop and this standalone analyzer.
const SPEC_TOP_DB = 0;
const SPEC_DYN = 90;
const F_MIN = 20;
const F_MAX = 20000;
// The classic 1-2-5 sequence — same set EqChart's own `GRID_HZ` uses, so the two charts' grids
// read as the same axis rather than two different conventions.
const FREQ_TICKS = [20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000];
// Evenly-spaced round numbers for `linear` mode instead — the 1-2-5 sequence above reads oddly
// non-uniform on a linear axis (bunched at the low end, exactly the thing linear mode exists to
// avoid). 2 kHz steps across the same [F_MIN, F_MAX] range.
const FREQ_TICKS_LIN = [20, 2000, 4000, 6000, 8000, 10000, 12000, 14000, 16000, 18000, 20000];

// hermiteTangent/traceSmooth moved to spline.ts — EqChart's spectrum backdrop grew the same
// per-bin-staircase-visible-as-jagged problem this trace solved and now shares the fix.

/** Parse a `#rrggbb` hex (the `--accent` CSS var) to [r,g,b]; a green phosphor fallback. */
function parseHex(hex: string): [number, number, number] {
  const m = hex.trim().match(/^#?([0-9a-f]{6})$/i);
  if (!m) return [80, 220, 140];
  const n = parseInt(m[1], 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
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
// Shown in an empty readout slot instead of leaving it blank — an empty chip popping in and out of
// existence every time the peak count changes (even just from frame-to-frame noise near a gate's
// threshold) reads as more of a glitch than a fixed-width dash sitting there quietly does.
const PEAK_PLACEHOLDER_HZ = "--- Hz";
const PEAK_PLACEHOLDER_DB = "--- dB";
// How close (in octaves) a candidate has to land to a tracked peak's last-known frequency to count
// as "the same peak, still there" rather than an unrelated one — see `trackPeaks`. Peak detection
// itself (prominence, the noise gate, harmonic folding, minimum separation) now runs backend-side
// (`cageq-monitor::find_peaks`, on the raw linear spectrum — see its own doc for why: this used to
// run here, on the log-binned display curve, and produced wildly wrong frequencies at the top of
// the spectrum), so this is deliberately the *tighter* of the backend's two resolution-dependent
// minimum separations (0.25 octaves at high-res), halved — safe in both modes rather than needing
// its own resolution branch just for this one derived tracking parameter. Loose enough to track a
// real peak's own frame-to-frame jitter (bin quantisation, a slow glide/vibrato), which is far
// smaller than that gap.
const PEAK_TRACK_MATCH_OCTAVES = 0.125;
// How long a tracked peak survives after nothing matches it before it's actually dropped — long
// enough to bridge an ordinary flicker right at a detection threshold (prominence, the noise gate,
// octave separation — a real peak sitting near any of those can wink out for a frame or two without
// the underlying content changing at all), short enough that a peak genuinely gone stops being
// reported promptly. In the same neighbourhood as cageq-monitor's own `PEAK_HOLD` (350ms) for a
// consistent feel — not the same value, since that one holds a meter *level*, this one holds an
// *identity*, but both answer "how long does a peak reading outlive the instant that produced it".
const PEAK_TRACK_HOLD_MS = 400;

/** A peak's identity across frames — its last-known frequency/level and when it last actually
 *  matched something in a fresh backend `peaks` list — kept by `trackPeaks`, independent of which
 *  readout chip it happens to be drawn into on any given tick (there is no fixed chip↔peak binding
 *  at all — see `trackPeaks`'s own doc for why). */
type TrackedPeak = { hz: number; db: number; lastSeen: number };

/** Update `tracked` against this tick's raw backend `peaks` (`SpectrumUpdate.peaks`,
 *  `cageq-monitor::find_peaks`) and return the peaks that should be shown right now, sorted
 *  ascending by frequency — always left-to-right in the order a reader scans the axis, exactly
 *  like the backend's own list order.
 *
 * The backend has no memory across snapshots: it recomputes the whole peak set from nothing every
 * time, and a real peak sitting near any of its thresholds (prominence, the noise gate, minimum
 * separation) can wink out for a frame or two. The readout used to map chip `j` straight to
 * `peaks[j]`, so a single flickering peak made *every other* chip's content jump too, not just its
 * own — reported live as the readout "shuffling" on ordinary, momentary content.
 *
 * The identity-matching idea is the same one already shipped for CAGEq's own live EQ push
 * (`SlotAssignment` in `cageq-apo-backend`): a tracked peak within `PEAK_TRACK_MATCH_OCTAVES` of a
 * candidate is the same peak, continuing; one nothing matches keeps existing (still reported, at
 * its last-known level — see the loop below) for `PEAK_TRACK_HOLD_MS` before it's actually dropped;
 * a genuinely new candidate starts a new tracked peak. Where this deliberately *diverges* from
 * `SlotAssignment` — first tried the same way, then corrected live ("it's visually harder to
 * follow [but] the readout should still be frequency sorted") — is display position:
 * `SlotAssignment` pins a band to a fixed slot index because that index is a real ramp target
 * something else depends on. Nothing downstream depends on which *chip* a peak lands in — the
 * crosses on the tube are positioned independently, straight from the backend list, not from this
 * — so there is no reason to trade the readout's left-to-right readability for a stability
 * property nothing needs. Sorting fresh each tick doesn't reintroduce the original jumping either:
 * a flickering peak is bridged by the hold instead of vanishing and reappearing, so its neighbours'
 * relative order — and hence position — never has to move for it; position only changes when the
 * tracked *set* genuinely changes (a peak truly arriving or, after its hold expires, truly
 * leaving), which is exactly when a reader would expect the row to move. */
function trackPeaks(
  tracked: TrackedPeak[],
  peaks: { hz: number; db: number }[],
  now: number,
  matchOctaves: number,
): TrackedPeak[] {
  const used = new Array(peaks.length).fill(false);

  // 1) Match each already-tracked peak to the closest still-unclaimed candidate, if one is near
  // enough to trust as "the same peak". Closest, not merely first-within-tolerance, so two tracked
  // peaks that both drifted toward the same gap don't race for whichever candidate they see first.
  for (const t of tracked) {
    let best = -1;
    let bestDist = matchOctaves;
    for (let k = 0; k < peaks.length; k++) {
      if (used[k]) continue;
      const dist = Math.abs(Math.log2(peaks[k].hz / t.hz));
      if (dist < bestDist) {
        best = k;
        bestDist = dist;
      }
    }
    if (best >= 0) {
      used[best] = true;
      t.hz = peaks[best].hz;
      t.db = peaks[best].db;
      t.lastSeen = now;
    }
    // else: left exactly as it was, level included — a held reading freezes at its last-known
    // level for the (short, PEAK_TRACK_HOLD_MS) grace period rather than tracking anything live,
    // since there's no local bin array left to re-read a fresher value from.
  }

  // 2) Drop whatever nothing has matched for too long.
  const next = tracked.filter((t) => now - t.lastSeen <= PEAK_TRACK_HOLD_MS);

  // 3) Whatever candidate is still unclaimed is a genuinely new peak — track it too, up to
  // PEAK_COUNT tracked at once (a peak beyond that simply doesn't get a chip this cycle).
  for (let k = 0; k < peaks.length; k++) {
    if (used[k]) continue;
    if (next.length >= PEAK_COUNT) break;
    next.push({ hz: peaks[k].hz, db: peaks[k].db, lastSeen: now });
  }

  next.sort((a, b) => a.hz - b.hz);
  return next;
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
export function SpectrumScope({
  legendHost,
  sampleRate,
  fftSize,
  onFftSizeChange,
  harmonicFold,
  onHarmonicFoldChange,
}: {
  /** The peak readout renders (via portal) into this element instead of inline below the tube —
   *  same mechanism, and the same element, as EqChart's own `legendHost` (App.tsx's
   *  `.chart-legend-host`, a sibling of `.chart-row`, outside `.chart-wrap` entirely). Keeps the
   *  readout out of the tube's own height budget — it used to sit inside `.spectrumscope-wrap` as a
   *  flex sibling of `.vs-screen`, which meant the tube was always shorter than `.chart-wrap`'s own
   *  full height by exactly the readout's height, unlike every other view's own screen. */
  legendHost?: HTMLElement | null;
  /** The real device sample rate, when known — see EqChart's identically-named prop for why this
   *  isn't cosmetic (the undistort correction's biquad math depends on it). Falls back to
   *  `biquad.ts`'s default 48 kHz only when genuinely unknown. */
  sampleRate?: number;
  /** The backend's analysis window size — one of `SPEC_FFT_SIZES` (`EqChart.tsx`), App.tsx-owned
   *  (not this component's own tune-panel params) because it's backend-side and global to the one
   *  running monitor, not per-viewer, so two open spectrum views must show the same slider
   *  position rather than each independently believing whichever they last set. The slider
   *  rendered here just reads/writes App.tsx's state via these two props, which also pushes the
   *  live `set_spectrum_fft_size` command — the running monitor reconfigures its analysis window
   *  in place (see cageq-monitor's `Spectrum::reconfigure`), no restart. See cageq-monitor's
   *  `BASE_FFT_SIZE`/`MED_FFT_SIZE`/`HIGH_RES_FFT_SIZE` docs for what each tier actually trades
   *  (resolution for temporal smearing, not CPU — CPU cost is negligible at all three). */
  fftSize?: number;
  onFftSizeChange?: (v: number) => void;
  /** Whether the backend's peak-finder (`cageq-monitor::find_peaks`) folds a harmonic series into
   *  its root — App.tsx-owned (not this component's own tune-panel params) for the same reason
   *  `fftSize` is: detection itself is now backend-side and global to the one running monitor, not
   *  per-viewer, so two open spectrum views must show the same checkbox state rather than each
   *  independently believing whichever they last set. The checkbox rendered here just reads/writes
   *  App.tsx's state via these two props, which also pushes the live `set_spectrum_harmonic_fold`
   *  command — same live-no-restart shape as `fftSize` now has.
   *
   *  Off by default: decluttering a harmonic series down to its fundamental is the right default
   *  for real program material (a mains hum's ladder, an instrument's own overtones), but it
   *  actively hides the thing a harmonic-rich test signal (`testtone`'s
   *  --square/--triangle/--sawtooth/--pulse, or the in-app generator) exists to show off —
   *  testtone.rs's own header doc says a clean --square readout "should show *only* clean odd
   *  harmonics", and folding does the opposite. `trackPeaks`'s own per-peak identity/hold already
   *  covers frame-to-frame *stability*, so folding's remaining job is pure decluttering, which
   *  isn't the right default now that reading individual harmonics is a real, common use of this
   *  view. */
  harmonicFold?: boolean;
  onHarmonicFoldChange?: (v: boolean) => void;
}) {
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
  // Refreshed every render, read fresh each frame inside the mount-once rAF loop below — same
  // pattern as `paramsRef`.
  const sampleRateRef = useRef(sampleRate);
  sampleRateRef.current = sampleRate;

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

  // Static graticule: a few dBFS reference lines + one vertical guide per tick (log: the classic
  // 1-2-5 sequence, matching EqChart's own grid; linear: evenly-spaced round numbers instead, see
  // FREQ_TICKS_LIN's own doc). Redrawn on resize or when the axis mode itself changes.
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
    const ticks = params.linear ? FREQ_TICKS_LIN : FREQ_TICKS;
    const tickX = (hz: number) => (params.linear ? ((hz - F_MIN) / (F_MAX - F_MIN)) * W : ((Math.log(hz) - lnMin) / lnSpan) * W);
    for (const hz of ticks) {
      const x = tickX(hz);
      ctx.moveTo(x, plotTop);
      ctx.lineTo(x, plotBot);
    }
    ctx.stroke();

    ctx.fillStyle = `rgba(${ar},${ag},${ab},0.5)`;
    ctx.font = `${Math.round(H * 0.045)}px system-ui, sans-serif`;
    ctx.textBaseline = "bottom";
    for (const hz of ticks) {
      const x = tickX(hz);
      // The tick set's own extremes (F_MIN/F_MAX) land exactly on the plot's edges — centring
      // their label there would run it half off-canvas, so those two anchor to the inside edge
      // instead; everything in between still centres on its gridline as before.
      ctx.textAlign = hz === F_MIN ? "left" : hz === F_MAX ? "right" : "center";
      ctx.fillText(hz >= 1000 ? `${hz / 1000}k` : `${hz}`, x, H - 2);
    }
  }, [resW, resH, params.linear]);

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
    // to it every idle frame for nothing. Was 120ms before `trackPeaks` — the old direct `peaks[j]`
    // indexing meant a slow rate also happened to hide some of the index-reshuffling jumps a faster
    // one would have caught more often; with peak identity itself stable now, that extra margin
    // isn't needed and the only remaining constraint is genuinely just digit-vibration. Halved as a
    // starting point, not a measured ideal — retune freely if it still reads as settled or as
    // vibrating at this rate.
    let lastReadout = 0;
    let hadPeak = false;
    const READOUT_INTERVAL_MS = 60;
    // Tracked-peak identity across ticks — see `trackPeaks`. Lives here (not a ref) for the same
    // reason `hadPeak` does: it belongs to this render loop's closure and should reset whenever the
    // effect itself re-runs (a device/param change is a clean slate, not something a tracked peak
    // should survive across).
    let tracked: TrackedPeak[] = [];
    // Whether the cursor label was showing last frame — same one-shot-hide idea as `hadPeak`, so
    // leaving the tube doesn't need a per-frame DOM write to keep confirming it's still hidden.
    let cursorShown = false;
    // Reused per-point scratch buffers for the trace (see `traceSmooth`) — resized, never
    // reallocated fresh each frame, matching TimeScope's `magScratch` pattern.
    let xScratch = new Float64Array(0);
    let yScratch = new Float64Array(0);
    // Every bin's (possibly corrected) value, one-to-one with xScratch/yScratch — the curve
    // actually drawn. Peak detection itself no longer reads this (see `SpectrumUpdate.peaks`'s own
    // doc) — it's backend-side, on the raw linear spectrum, precisely because this array's
    // Gaussian-density smoothing dilutes and reshapes a high-frequency tone too much to locate one
    // accurately.
    let vScratch = new Float64Array(0);
    // Same bins' *true* linear-FFT level (`peak_db` — max within the bin's span, not the
    // Gaussian-weighted density average `db`/vScratch uses) — one-to-one with vScratch, read only
    // for the hover cursor's numeric readout below (the peak readout reads the backend's own
    // already-refined per-peak level, `s.peaks`, instead). The two exist because they answer
    // different questions: vScratch is "what does the spectral *density* look like" (correct shape
    // for broadband content, ~19dB-droops a swept tone by design — see `gaussian_power`'s doc in
    // cageq-monitor), pScratch is "what is the actual level right here" (correct for an isolated
    // tone, noisier as a *shape* for broadband content — exactly why it's never the thing stroked).
    let pScratch = new Float64Array(0);
    // Exponentially-smoothed copy of yScratch actually drawn — see the render loop's own comment
    // (the window-drag stutter fix). NaN marks "not yet initialized" so a fresh bin snaps straight
    // to target instead of animating in from zero.
    let yScratchSmooth = new Float64Array(0).fill(NaN);

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
      const n = (p.linear ? s?.db_lin.length : s?.db.length) ?? 0;
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
        const corr = p.undistort ? getCorrection(corrCacheRef, eqRef.current, s, sampleRateRef.current, dt * 1000, p.linear) : null;
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
          pScratch = new Float64Array(n);
          const grown = new Float64Array(n).fill(NaN);
          grown.set(yScratchSmooth); // preserve already-settled bins; new ones start at NaN (unset)
          yScratchSmooth = grown;
        }
        // One point per bin, no deduplication — see `hermiteTangent`'s doc for why that's safe now:
        // the backend's Gaussian reduction essentially never produces two adjacent bins with the
        // exact same value the way the old `max`-based one routinely did, so there's no "tied run"
        // left to collapse or preserve the shape of.
        const srcDb = p.linear ? s.db_lin : s.db;
        const srcPeakDb = p.linear ? s.peak_db_lin : s.peak_db;
        for (let i = 0; i < n; i++) {
          vScratch[i] = corr ? srcDb[i] - corr[i] : srcDb[i];
          pScratch[i] = corr ? srcPeakDb[i] - corr[i] : srcPeakDb[i];
          const frac = Math.max(0, Math.min(1, (vScratch[i] - (SPEC_TOP_DB - SPEC_DYN)) / SPEC_DYN));
          // Bin index maps straight to pixel X in both axis modes — bins are already
          // uniformly spaced along whichever axis is active (log or linear); only the
          // bin-to-Hz mapping used elsewhere (peak crosses, the hover cursor, the grid) differs.
          xScratch[i] = (i / (n - 1)) * W;
          yScratch[i] = plotBot - frac * (plotBot - plotTop);
        }
        // Light exponential smoothing toward the raw target — see STROKE_SMOOTH_TAU's own doc for why
        // (the window-drag stutter fix). NaN (unset, a fresh bin) snaps straight to target instead of
        // animating in from zero.
        const smoothK = Math.exp(-dt / STROKE_SMOOTH_TAU);
        for (let i = 0; i < n; i++) {
          yScratchSmooth[i] = Number.isNaN(yScratchSmooth[i]) ? yScratch[i] : yScratch[i] + (yScratchSmooth[i] - yScratch[i]) * smoothK;
        }
        // n >= 2 already guaranteed by the outer `if`.
        ctx.beginPath();
        traceSmooth(ctx, xScratch, yScratchSmooth, n);
        ctx.stroke();

        // Peak crosses: recomputed and redrawn every frame (not throttled — see below), so they
        // track the live trace exactly as fluidly as the trace itself does. (Layer already cleared
        // above, unconditionally.) Peaks themselves are backend-computed (`s.peaks`,
        // `cageq-monitor::find_peaks`, on the raw linear spectrum, independent of either display
        // axis) — this used to run client-side on `vScratch` (the log-binned, Gaussian-smoothed
        // curve), which produced wildly wrong frequencies at the top of the spectrum; see that
        // function's own doc. `hzToFrac` inverts whichever axis the curve itself is drawn on (log
        // or linear — see `Params.linear`'s doc) to place an already-resolved Hz value on the
        // canvas.
        const lnF0 = Math.log(s.f_min);
        const lnSpan = Math.log(s.f_max) - lnF0;
        const hzToFrac = (hz: number) => (p.linear ? (hz - s.f_min) / (s.f_max - s.f_min) : (Math.log(hz) - lnF0) / lnSpan);
        // The backend has no notion of "undistort" — its peaks are always the raw, post-EQ
        // spectrum's own. When undistort is on, the drawn curve subtracts `corr` (the EQ's own
        // response) to show the reconstructed pre-EQ signal, so a raw peak's *level* is adjusted
        // to match here too — interpolated into `corr`'s bins the same way the hover cursor's own
        // dB readout is, below. This is a display-level correction only: which frequencies the
        // backend picked as peaks was already decided on the uncorrected spectrum, a
        // simplification accepted because a real EQ correction curve is broad and smooth relative
        // to genuine peaks, not something that plausibly manufactures or hides one.
        const rawPeaks = s.peaks ?? [];
        const peaks = corr
          ? rawPeaks.map((pk) => {
              const fi = Math.max(0, Math.min(n - 1, hzToFrac(pk.hz) * (n - 1)));
              const i0 = Math.floor(fi);
              const i1 = Math.min(n - 1, i0 + 1);
              const t = fi - i0;
              return { hz: pk.hz, db: pk.db - (corr[i0] * (1 - t) + corr[i1] * t) };
            })
          : rawPeaks;
        if (peaks.length) {
          markCtx.strokeStyle = PEAK_MARK_COLOR;
          markCtx.lineWidth = Math.max(1, H / REF_SIZE) * 1.5;
          const r = Math.max(3, H * 0.018); // cross arm length
          markCtx.beginPath();
          for (const pk of peaks) {
            const frac = Math.max(0, Math.min(1, (pk.db - (SPEC_TOP_DB - SPEC_DYN)) / SPEC_DYN));
            const x = hzToFrac(pk.hz) * W;
            const y = plotBot - frac * (plotBot - plotTop);
            markCtx.moveTo(x - r, y);
            markCtx.lineTo(x + r, y);
            markCtx.moveTo(x, y - r);
            markCtx.lineTo(x, y + r);
          }
          markCtx.stroke();
        }

        // The readout row's text, throttled independently of the (unthrottled) crosses above — see
        // `lastReadout`'s own comment. `trackPeaks` gives each peak identity across ticks (see its
        // own doc) instead of the old direct `peaks[j]` indexing, so a peak flickering near a
        // detection threshold no longer makes the whole row jump — and it's still presented sorted
        // by frequency, same as the backend's own list order, so left-to-right still reads as the
        // tube's own frequency axis.
        if (now - lastReadout > READOUT_INTERVAL_MS) {
          lastReadout = now;
          hadPeak = peaks.length > 0;
          tracked = trackPeaks(tracked, peaks, now, PEAK_TRACK_MATCH_OCTAVES);
          for (let j = 0; j < PEAK_COUNT; j++) {
            const slot = peakSlotRefs.current[j];
            const hzSpan = peakHzRefs.current[j];
            const dbSpan = peakDbRefs.current[j];
            if (!slot || !hzSpan || !dbSpan) continue;
            if (j < tracked.length) {
              const t = tracked[j];
              hzSpan.textContent = fmtPeakHz(t.hz);
              dbSpan.textContent = `${t.db.toFixed(1)} dB`;
              // Same Fc→hue mapping ToneGrid's Fc readout uses, at the same full strength — a
              // peak's frequency reads as the same colour here as a band tuned to it would there.
              slot.style.color = fcHue(t.hz);
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
        // the DOM readout text. Tracked identity is dropped too — a real signal gap is not the
        // momentary flicker `trackPeaks`'s grace period exists to bridge, and resuming should start
        // from a clean slate rather than let a stale tracked peak claim whatever turns up first.
        hadPeak = false;
        tracked = [];
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

        const hz = p.linear
          ? F_MIN + hoverFrac * (F_MAX - F_MIN)
          : Math.exp(Math.log(F_MIN) + hoverFrac * (Math.log(F_MAX) - Math.log(F_MIN)));
        let dbText = PEAK_PLACEHOLDER_DB;
        if (n >= 2 && s && s.signal) {
          // Linear interpolation between the two bins straddling the cursor — reads the underlying
          // data directly rather than the cosmetically-smoothed spline drawn through it (`traceSmooth`),
          // which is the right choice for a readout: the spline's only job is to look good between
          // points, not to claim sub-bin structure the data itself doesn't have. pScratch, not
          // vScratch: the true (undiluted) linear-FFT level at the cursor, not the density-
          // smoothed one the trace is drawn from — the same distinction `SpectrumUpdate::peak_db`'s
          // own doc makes, though the peak readout itself now reads the backend's already-refined
          // per-peak level (`s.peaks`) directly rather than this array.
          const fi = hoverFrac * (n - 1);
          const i0 = Math.floor(fi);
          const i1 = Math.min(n - 1, i0 + 1);
          const t = fi - i0;
          dbText = `${(pScratch[i0] * (1 - t) + pScratch[i1] * t).toFixed(1)} dB`;
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
      phos.commit(dt, p.trailTau, p.tail, TAU_REF / p.trailTau, p.bloom, p.haze);
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
  type NumKey = "trailTau" | "tail" | "glow" | "bloom" | "haze";
  const CONTROLS: { key: NumKey; label: string; min: number; max: number; step: number }[] = [
    { key: "trailTau", label: t("scope.trail"), min: 0.02, max: 0.6, step: 0.01 },
    { key: "tail", label: t("scope.tail"), min: 1, max: 64, step: 1 },
    { key: "glow", label: t("scope.glow"), min: 0.02, max: 1, step: 0.02 },
    { key: "bloom", label: t("scope.bloom"), min: 0, max: 2, step: 0.05 },
    { key: "haze", label: t("scope.haze"), min: 0, max: 4, step: 0.05 },
  ];

  return (
    <>
    <div className="spectrumscope-wrap">
      {/* width/height here are CSS "100%" (matching this box exactly, sub-pixel precise, since its
          own size comes from `.spectrumscope-wrap .vs-screen`'s own `width:100%;height:100%` rather
          than an inline style) — the JS-measured `resW`/`resH` state feeds only the canvas
          backing-store *resolution* attributes below, never the display size. A JS-measured,
          floored pixel value here would drift by up to 1px from the box's true (CSS-computed)
          height and show up as exactly the kind of small persistent misalignment against EqChart's
          own SVG sizing. */}
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
            <label className="vs-tune-row vs-tune-check" title={t("scope.linearHint")}>
              <span className="vs-tune-label">{t("scope.linear")}</span>
              <input type="checkbox" checked={params.linear} onChange={(e) => set("linear", e.currentTarget.checked)} />
            </label>
            {onHarmonicFoldChange && (
              <label className="vs-tune-row vs-tune-check" title={t("scope.harmonicFoldHint")}>
                <span className="vs-tune-label">{t("scope.harmonicFold")}</span>
                <input
                  type="checkbox"
                  checked={!!harmonicFold}
                  onChange={(e) => onHarmonicFoldChange(e.currentTarget.checked)}
                />
              </label>
            )}
            {onFftSizeChange && (
              <label className="vs-tune-row" title={t("scope.fftSizeHint")}>
                <span className="vs-tune-label">{t("scope.fftSize")}</span>
                <input
                  type="range"
                  min={0}
                  max={SPEC_FFT_SIZES.length - 1}
                  step={1}
                  value={fftSizeIndex(fftSize)}
                  onChange={(e) => onFftSizeChange(SPEC_FFT_SIZES[Number(e.currentTarget.value)])}
                />
                <b>{t(`scope.fftSizeTier${fftSizeIndex(fftSize)}`)}</b>
              </label>
            )}
          </div>
        )}
      </div>
    </div>
    {/* Numeric peak readout — up to PEAK_COUNT chips, one per cross marked on the tube, presented
        left-to-right by frequency (see `SpectrumUpdate.peaks`/`cageq-monitor::find_peaks`). The
        trail's own long afterglow (`tail`,
        phosphor.ts) already shows *where* the spectrum has recently been, but reading an exact
        level or frequency off a glowing curve isn't realistic — this is the same information as a
        number. A fixed PEAK_COUNT of slots is rendered upfront, ALWAYS all PEAK_COUNT of them (see
        App.css — no more collapsing an empty slot to `display:none`), so the row's own width and
        each chip's own position never depend on how many peaks are currently found; and each chip
        is two independently-sized fixed-width fields (frequency, level — App.css again) rather than
        one free-width text node, so a peak sliding from "60 Hz" to "8.2 kHz" can't shift every chip
        after it sideways either. An empty slot shows PEAK_PLACEHOLDER_HZ/DB ("--- Hz"/"--- dB")
        rather than going fully blank, for the same reason: a chip popping between text and nothing
        every time the peak count changes reads as a glitch, where a quiet dash sitting in a
        fixed-width field doesn't. Slots are filled or reset to the placeholder by writing their
        text (see the trail effect) rather than mapping over a variable-length array, since the
        array itself lives outside React state. Updated imperatively, NOT via React state:
        `spectrum` arrives well above React's comfortable render rate, and driving a state update
        from it once re-rendered the entire App tree per arrival (see cageq-monitor's
        `SpectrumUpdate` doc / this component's own history) — the fix there was moving the data off
        state entirely, so adding a state-driven readout here would reintroduce exactly that.

        Portaled into `legendHost` (App.tsx's `.chart-legend-host`, outside `.chart-wrap` entirely —
        same mechanism EqChart's own legend uses), not rendered inline below the tube any more: it
        used to sit inside `.spectrumscope-wrap` as a flex sibling of `.vs-screen`, which made the
        tube always shorter than `.chart-wrap`'s own full height by exactly this row's height —
        unlike every other view's own screen, which fills the box completely. See `legendPortal`'s
        own doc for the (host-less) inline fallback this never actually takes in practice, since
        App.tsx always supplies `legendHost`. */}
    {legendPortal(
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
      </div>,
      legendHost,
    )}
    </>
  );
}

/** Render into `host` via a portal when provided, else inline (mirrors EqChart.tsx's identically
 *  named helper — small enough that duplicating it across the two chart views that need it is
 *  cheaper than a shared module for one three-line function). */
function legendPortal(markup: ReactNode, host?: HTMLElement | null) {
  return host ? createPortal(markup, host) : markup;
}
