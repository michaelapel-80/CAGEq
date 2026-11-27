import { useEffect, useRef, useState } from "react";
import { FrameCapRow, frameGate } from "./frameLimit";
import { frameClock } from "./frameClock";
import { useTranslation } from "react-i18next";
import { listen, emit } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { type FadingInverse, retargetFadingInverse, stepFadingInverse } from "./biquad";
import { detectorSample, findRisingEdge, triggerDetector, type TriggerDetector } from "./scopeTrigger";
import { scopeReader } from "./streams";
import type { ScopeReader } from "./scopeClock";
import { accentRgb, createPhosphor, frameDoseMult, retuneForRelativeContrast, type TraceSegments } from "./phosphor";
import { LANCZOS_A, LANCZOS_TAPS, lanczosAt, lanczosRow, MAX_PIECES, PIECE_PX } from "./beamPath";
import { type ParamsMigration, useTunableParams } from "./useTunableParams";
import { APP_ZOOM_EVENT, beamScale, noteScopeTube } from "./appZoom";
import type { ScopeEq } from "./Vectorscope";

/** Live-tunable render parameters (see Vectorscope's identical rationale — a live panel beats a
 *  recompile). `undistort` shares Vectorscope's meaning and machinery: it undoes the loudness-match
 *  preamp *and* inverse-filters the applied EQ, recovering the pre-EQ level and shape rather than
 *  just amplifying the post-EQ trace — a manual gain knob would need re-tuning every time the
 *  preamp changes (a different headphone/target/slot), this tracks it automatically. `msPerDivIdx`
 *  is an index into `MS_PER_DIV_STEPS` (a classic scope-dial time/div, not a free-form span) —
 *  decoupled from the backend's ~16 ms emission size once a ring buffer is needed for triggering
 *  anyway, so it's exposed in both modes, not just when triggered; there's deliberately no
 *  longer-timescale/envelope mode, this stays a short-timescale instrument.
 *  `trigger`/`triggerFilterHz` and `mode` (L/R⇄Mix) are surfaced separately, as quick toolbar
 *  toggles rather than panel sliders — they're mode switches flipped often, not fine-tuning knobs —
 *  but they're still ordinary `Params` fields underneath, so "Save as default"/"Reset" (the tuning
 *  panel's own buttons) cover them exactly like everything else. */
type Params = {
  trailTau: number;
  tail: number;
  glow: number;
  beam: number;
  bloom: number; // tight-radius glow intensity (phosphor.ts's bloomIntensity) — 0 = off; see
  //                Vectorscope.tsx's own Params doc, tried there first
  haze: number; // broad, faint glow intensity (phosphor.ts's bloomWide) — 0 = off
  saturation: number; // the phosphor's saturation (phosphor.ts's setTone) — 0 = linear
  contrast: number; // the display response, intensity^contrast (phosphor.ts's setTone) — 1 = linear
  headroom: number; // how far past the display's white the phosphor saturates (phosphor.ts's setTone) — 1 = at white
  bloomWhite: number; // the bloom's colour, 0 the beam's hue to 1 white (phosphor.ts's setTone)
  bwLimit: number; // the GPU trace's bandwidth limit, a Gaussian low-pass this many px wide — 0 = off
  speedDim: number; // how much the GPU beam dims where it moves fast — 1 = a real beam's, 0 = a drawn line's
  undistort: boolean;
  msPerDivIdx: number;
  trigger: boolean;
  triggerFilterHz: number;
  mode: "lr" | "mix";
};
// Classic 1-2-5 time/div sequence, same convention a real scope's dial steps through. 10 divisions
// (DIVISIONS) is the standard horizontal graticule count, so total span = ms/div × 10.
const MS_PER_DIV_STEPS = [0.2, 0.5, 1, 2, 5, 10];
const DIVISIONS = 10;
// The GPU beam's path between samples is reconstructed (beamPath.ts). Here, at most DENSE_PIECES
// per interval, or one per pixel of its width where samples sit further apart, up to MAX_PIECES:
// dense content (pink noise at 2-8 samples/px) is within 4% of a 16x reconstruction at 4 pieces
// (straight chords: 20-30% off), while sparse samples (a few kHz at 0.2 ms/div) need ~8-16 — cheap
// there, the window being few samples. Measured cost per window, both lanes: ~0.3 ms for a tone,
// ~0.6 ms for dense content at the default 50 ms, ~1.8 ms at 100 ms of 96 kHz.
const DENSE_PIECES = 4;
// The bandwidth limit's smallest sigma worth filtering with, in samples (see bwLimit): below it the
// kernel is all but a single tap.
const BW_MIN_SIGMA = 0.3;
// Above this sigma (samples) the Gaussian is approximated by three running box sums instead (see
// boxBlur): their cost doesn't grow with the width, where direct taps at the longest timebase
// (sigma ~23 samples at 100 ms of 96 kHz, 3 px) cost ~3 ms a window. Three boxes come within a few
// percent of a Gaussian, and a box is all-positive too, so still no overshoot.
const BW_DIRECT_MAX_SIGMA = 3;

/** Box widths (odd) for `passes` running box sums approximating a Gaussian of standard deviation
 *  `sigma` — the usual mix of two adjacent odd widths, chosen to match the variance. */
function boxesForGauss(sigma: number, passes: number): number[] {
  let wl = Math.floor(Math.sqrt((12 * sigma * sigma) / passes + 1));
  if (wl % 2 === 0) wl--;
  const m = Math.round((12 * sigma * sigma - passes * wl * wl - 4 * passes * wl - 3 * passes) / (-4 * wl - 4));
  return Array.from({ length: passes }, (_, i) => (i < m ? wl : wl + 2));
}

/** One running box sum of odd width `w` over `src[0, n)` into `dst`, ends held. */
function boxBlur(src: Float32Array, dst: Float32Array, n: number, w: number): void {
  const r = (w - 1) / 2;
  const at = (k: number) => src[Math.min(n - 1, Math.max(0, k))];
  let acc = 0;
  for (let k = -r; k <= r; k++) acc += at(k);
  for (let k = 0; k < n; k++) {
    dst[k] = acc / w;
    acc += at(k + r + 1) - at(k - r);
  }
}
// glow/beam/bloom/haze/saturation/contrast/headroom/bloomWhite/speedDim: tuned live on the GPU beam
// in linear light (see PhosphorOptions' `linearLight`, `Phosphor.setTone`). Saturation lets a dense
// trace's slow points roll off rather than standing out as dots; speedDim below 1 keeps dense
// content a readable stroke rather than a speckle (see where it's applied).
// bwLimit off: dense content can get a bit grainy without it, which the tuning above mostly
// mitigates, but a moving signal is harder to follow with it on. Left in as a setting.
// triggerFilterHz 60: a lower cutoff locks more steadily (fewer harmonics to hop between). It used
// to sit at 120 because low cutoffs stopped triggering at all: the filtered waveform shrinks, and
// any near-DC content then held it clear of the zero level. The AC-coupled level (scopeTrigger.ts)
// removed that, so it now triggers reliably down to the knob's 20 Hz minimum.
//
// trailTau/tail/glow: the Fast preset's values. The steady trigger leaves no smear for a trail to
// hide, so an instant, unsmeared trace is the better first view; the former defaults live on as
// the Smooth preset.
const DEFAULTS: Params = {
  trailTau: 0.02,
  tail: 3,
  glow: 0.8,
  beam: 0.6,
  bloom: 0.35,
  haze: 0.1,
  saturation: 4.0,
  contrast: 1.4,
  headroom: 8.0,
  bloomWhite: 0.25,
  bwLimit: 0.0,
  speedDim: 0.75,
  undistort: true,
  msPerDivIdx: 4, // 5 ms/div × 10 = 50 ms
  trigger: true,
  triggerFilterHz: 60,
  mode: "mix",
};
// Curated Trail/Tail/Glow combinations — see SpectrumScope's own PRESETS doc for the full
// reasoning (shared verbatim: the Trail/Glow orthogonality fix keeps *steady-state* brightness
// independent of trailTau in theory, but live tuning showed glow still needs its own adjustment
// per preset). Here, "distribution" mainly pays off in free-run mode on dense/non-periodic
// material, where a long window overlays many cycles into a waveform-density cloud rather than one
// blurred average trace — the preset that gains most from linear light, its faint density no
// longer crushed to black. All three tuned live in linear light. In button order, shortest
// persistence first.
const PRESETS = {
  fast: { trailTau: 0.02, tail: 3, glow: 0.8 },
  smooth: { trailTau: 0.04, tail: 6, glow: 0.8 },
  distribution: { trailTau: 0.8, tail: 64, glow: 4.0 },
} satisfies Record<string, Pick<Params, "trailTau" | "tail" | "glow">>;
// Stored settings from before the GPU beam's linear light (v1): glow, bloom and haze meant
// something else there (energy shown as an encoded value, a thresholded bloom), so they take the
// new defaults rather than carrying numbers over that no longer mean the same. The rest (trail,
// beam, timebase, trigger) keeps its meaning. Version 2: contrast relative to the trace's colour —
// linear-light settings (v1) brought along by retuneForRelativeContrast.
const TIMESCOPE_PARAMS_MIGRATION: ParamsMigration = {
  version: 2,
  migrate: (stored, from) =>
    from < 1
      ? { ...stored, glow: DEFAULTS.glow, bloom: DEFAULTS.bloom, haze: DEFAULTS.haze }
      : from < 2
        ? retuneForRelativeContrast(stored, DEFAULTS.contrast)
        : stored,
};
const PRESET_LABELS: Record<keyof typeof PRESETS, string> = { fast: "scope.presetFast", smooth: "scope.presetSmooth", distribution: "scope.presetDistribution" };
const REF_SIZE = 512; // beam width authored against this reference height, then scaled
/** localStorage keys: the detached time scope's own L/R vs mixdown and speed dimming (see `popMode`). */
const POPOUT_MODE_KEY = "cageq-timescope-popout-mode";
const POPOUT_SPEED_DIM_KEY = "cageq-timescope-popout-speed-dim";
// The pop-out's speed dimming: the real beam's. The inline default (DEFAULTS.speedDim) is below 1
// because a short tube packs dense content into a speckle; the pop-out has the room to draw it as
// the beam actually would.
const POPOUT_SPEED_DIM_DEFAULT = 1;
const readStored = <T,>(key: string, parse: (v: string | null) => T, fallback: T): T => {
  try {
    return parse(localStorage.getItem(key));
  } catch {
    return fallback;
  }
};
const writeStored = (key: string, v: string) => {
  try {
    localStorage.setItem(key, v);
  } catch {
    // Not remembered, still applied.
  }
};
const GRID_ALPHA = 0.22;
const DIV_LINE_ALPHA = GRID_ALPHA * 0.6; // division ticks read as finer/subtler than the lane centrelines
// Peak-hold ballistics for the faint clip-reference lines — instant attack, brief hold, then
// linear-dB release (a PPM-style follower, same shape as the level meter's — values mirror
// cageq-monitor's PEAK_RELEASE_DB_PER_SEC/PEAK_HOLD/DB_FLOOR for a consistent feel app-wide).
// Tracked locally per lane from the *displayed* samples (not the meter's own value) so it stays
// correct in both modes: with undistort on, the meter's raw-output peak wouldn't match a trace
// that's now been amplified/reshaped — the line would sit *inside* the beam it's supposed to
// reference. Tracking the same outL/outR the trace itself draws is correct by construction.
const PEAK_RELEASE_DB_PER_SEC = 20;
const PEAK_HOLD_MS = 1200;
const DB_FLOOR = -120;
// The release stops at what the signal reads now (the latest window's reading), not below it: the
// hold only re-arms on a reading at or above the line, so a steady tone whose readings wobble a hair
// under the one that set it (0.001 dB, with undistort on) let the hold lapse every second or so,
// and the line fell until the next window pushed it back — a dip of up to 0.44 dB for a frame or
// four (logged live). A falling signal is still followed down, at the release rate. A reading older
// than PEAK_READING_STALE_MS no longer holds the line up: the stream sends no windows in silence,
// and the line has to release then.
const PEAK_READING_STALE_MS = 50;
// A trace on every frame, with a frame's worth of dose: the playout clock (scopeClock.ts) gives each
// frame its own stretch of audio, and on a frame it has none for (a late window: "starved") the
// last trace is drawn again, as a scope re-sweeping a steady signal would show it. Drawn only on
// frames a window had arrived in, the Fast preset's 20 ms trail decayed in every gap: a steady
// signal's brightness swung 20–26% RMS frame to frame (simulated against the old stream's
// timing). Held up to TRACE_HOLD_MS; past that, or once the stream goes idle, the trace fades.
const TRACE_HOLD_MS = 100;
// Peak *detection* takes the K-th largest rectified sample in the block, not the plain max and
// not a windowed/smoothed value — a sustained tone has many samples near its own peak, one every
// cycle for as long as it lasts, scattered across the whole block rather than adjacent; an
// isolated glitch — lossy-codec pre/post-echo artifacts, or ringing from the undistort inverse
// filter (a deep EQ cut inverts into a resonant boost, see inverseBiquadCoeffs in biquad.ts, which
// decays cycle to cycle rather than sustaining) — only ever has a handful, clustered right at its
// start. Requiring K corroborating samples *anywhere* in the block (not contiguous) tells the two
// apart correctly regardless of frequency: two earlier attempts at a contiguous window — a
// one-pole envelope, then a sliding-window erosion — both discounted *every* brief event in
// proportion to its width, so widening either to catch glitches also read genuine high-frequency
// content low (its rectified envelope dips near zero every half-cycle, same as a glitch decaying,
// at any window wide enough to matter). Expressed as a duration (PEAK_CONFIRM_MS → K = samples in
// that many ms) so it's identical in effect at 44.1/48/96/192 kHz. Only affects this reference
// line — the drawn trace itself is untouched.
const PEAK_CONFIRM_MS = 1.0;
// …over the last PEAK_SPAN_MS of audio, read from the trigger ring, not over each scope window on
// its own: the windows aren't a fixed length — each carries however many of the loopback's 10 ms
// packets arrived since the last ~16 ms emit, mostly one or two — and with K fixed, how far down
// the rectified signal's top K reaches depends on how much signal there is to rank. A band-limited
// 500 Hz square at 48 kHz has 4 samples per period on its Gibbs crest (+1.42 dB), the next 4 at
// +0.52, the next at +0.28: K = 48 read +0.28 dB in a one-packet window and +0.52 in a two-packet
// one, so every two-packet window lifted the line, which held and then fell back — a jump on a
// steady tone. Over a fixed span a steady square reads within 0.09 dB at any frequency (simulated,
// 440 Hz to 5 kHz; the per-window rule swung up to 0.86). 16 ms: the stream's own window, so K
// still asks for what it was tuned to ask for.
const PEAK_SPAN_MS = 16;
// Peak-line opacity. These used to be drawn *into* the accumulating trail every frame with
// source-over at 0.35, which doesn't stay at 0.35: re-blending over the faded previous frame
// converges on L = 0.35·C + 0.65·(L·keep), i.e. ~0.65·C at the default trail (and brighter still at
// longer ones — the reference line's brightness silently tracked a cosmetic slider). Drawn once per
// frame onto a cleared layer it would be exactly 0.35·C, about half as bright, which is what made
// them look washed out after the move. This is that convergence point, now fixed and predictable
// instead of trail-dependent.
const PEAK_LINE_ALPHA = 0.65;

// --- Triggering ---------------------------------------------------------------------------
// A stable oscilloscope-style trigger needs pre/post-trigger history the raw ~16 ms `scope`
// windows alone don't provide, so incoming samples are appended to a ring buffer (raw display
// samples + a persistently-filtered mono trigger-detector signal, both written incrementally as
// data streams in — never re-filtered from scratch each frame, which would both waste work on the
// overlapping portion and reset the filter's state every frame, corrupting its settling).
const RING_CAP = 1 << 17; // ~2.7 s at 48 kHz / ~680 ms at 192 kHz — generous, trivial memory
// Where the trigger point sits across the display window, in whole divisions so it lands on a
// graticule line (it used to be a quarter of the window: 2.5 divisions, between two lines).
const TRIGGER_PRE_DIVS = 2;
const TRIGGER_SEARCH_MS = 250; // how far back to look for a qualifying edge before giving up
// How long one edge may go on being drawn, in audio arriving after it was first drawn, ms. Until a
// newer edge comes along the search keeps finding the same one, and the picture then stands still:
// rarely, on content where the lock fails, for a few hundred ms (reported live), while the music
// goes on. Past this the scope free-runs until there's a new edge, as a real scope's auto mode does.
// Not on the first repeat: bass legitimately has no newer edge for a while (a 25 Hz tone repeats
// its edge for ~2 frames at 60 fps, the picture identical anyway), so the hold covers the longest
// period the knob passes — 50 ms at its 20 Hz minimum.
const TRIGGER_HOLD_MS = 60;
// When the trigger's cutoff (or the rate) changes, the history the search can reach is run through
// the new detector again, so the change has no settling transient: the search span, the longest
// display window, and the forward pass's run-in.
const TRIGGER_REFILTER_MS = TRIGGER_SEARCH_MS + 100 + 100;
// The detector itself (zero-phase low-pass, AC-coupled level, settling) lives in scopeTrigger.ts.

// Trail/Glow orthogonality: at steady state (a dose landing every `dtSinceTrace` seconds, decaying
// at `exp(-dt/trailTau)` between doses), accumulated brightness is approximately
// `dose_per_second * trailTau` (see phosphor.ts's DOSE_REF_FPS doc for the same derivation) — so
// *only* moving Trail longer measurably brightens a steady/dwelling signal even with Glow untouched,
// reported live as the two sliders reading as coupled rather than independent controls (persistence
// length vs. brightness, which is what they're meant to be). `TAU_REF / trailTau` in doseMult below
// cancels that trailTau term. Anchored at a real 100 ms, not at this view's own (arbitrary, prone to
// drifting with the next re-tune) default trailTau: with that anchor, `glow` reads as "steady-state
// brightness units per 100 ms of persistence" — a fixed, physically meaningful reference instead of
// an implementation detail.
//
// First attempt anchored at a full second (TAU_REF=1), which is *also* physically meaningful but
// shrinks `glow` by trailTau/TAU_REF — here, 0.06/1 = 16.7x. That shrunk value is what gets drawn
// onto the 8-bit 2D scratch canvas *before* doseMult's float-precision correction ever applies — the
// dimmest content rounds to just 0 or 1 out of 255, so it flips unpredictably between invisible and
// (once dose-multiplied back up) suddenly fully saturated. Reported live (on Vectorscope, same
// mechanism here) as reproducible, sharply-banded artifacts on periodic content. 100ms shrinks
// `glow` by only 0.06/0.1 = 0.6x here (i.e. barely at all), staying comfortably clear of that
// quantisation cliff while still being a fixed, non-arbitrary reference. DEFAULTS.glow below is
// `old_glow * old_trailTau / TAU_REF` (0.50 * 0.06 / 0.1 = 0.30), chosen so the shipped default
// renders identically to before the original decoupling change — only the number's meaning moved,
// twice now. (A third time with the GPU beam's linear light — see DEFAULTS and FALLBACK_GLOW_SCALE.)
const TAU_REF = 0.1;
// The Canvas 2D fallback still strokes at alpha = glow, as before linear light: scaled back by the
// factor glow's default moved by (0.2 -> 0.8), so it keeps the look it had.
const FALLBACK_GLOW_SCALE = 0.2 / 0.8;

/** Lane geometry (vertical centre + amplitude scale + label) for the current channel mode — shared
 *  between the graticule and the trace so they always agree. `amp` leaves a small margin (0.5 of
 *  the lane's half-height) so a full-scale sample doesn't touch the lane's edge/divider. */
type Lane = { cy: number; amp: number; label: string | null };
function laneLayout(mode: "lr" | "mix", H: number): Lane[] {
  if (mode === "mix") return [{ cy: H / 2, amp: H * 0.5, label: null }];
  const laneH = H / 2;
  return [
    { cy: laneH / 2, amp: laneH * 0.5, label: "L" },
    { cy: laneH + laneH / 2, amp: laneH * 0.5, label: "R" },
  ];
}

/** The K-th largest of `mag[0..n)` (rectified samples) — the peak-detection core described at
 *  `PEAK_CONFIRM_MS` above. `k = 1` is the plain max; `k` clamps to `n` for a block shorter than
 *  the confirmation window, which just falls back to the max rather than under-reading. Sorts
 *  `mag`'s first `n` entries in place (ascending — `%TypedArray%.sort()`'s numeric default, unlike
 *  `Array`'s lexicographic one, so no comparator needed) — the caller's scratch buffer, so it's
 *  fine to mutate; cheap at these block sizes (a handful of thousand samples at most) and simpler
 *  than a partial-selection algorithm for no measurable cost at this rate. */
function kthLargest(mag: Float64Array, n: number, k: number): number {
  if (n === 0) return 0;
  const view = mag.subarray(0, n);
  view.sort();
  return view[Math.max(0, n - Math.min(k, n))];
}


/**
 * Time-domain scope (v2, with triggering): the loopback's L/R
 * channels plotted against time, stacked in two lanes (L on top, R on bottom), each with its own
 * zero-centreline, phosphor-style persistence. `trigger` off (default) is free-running — a fixed
 * span (ms/div × DIVISIONS) of the most recent samples, redrawn each `scope` event; on non-periodic material
 * (most real mixes) consecutive windows won't retrace the same shape, so persistence reads as a
 * soft blur rather than a locked waveform. `trigger` on searches a zero-phase lowpass-filtered mono
 * version of the signal (HF-reject trigger coupling — the display itself stays full-bandwidth) for
 * the most recent rising crossing of its own local mean (AC trigger coupling, so a near-DC offset
 * can't hold the waveform clear of the level; see scopeTrigger.ts) within `TRIGGER_SEARCH_MS`,
 * anchoring the display window there
 * instead; no qualifying edge in range falls back to the same free-running position (the auto-
 * trigger/timeout behaviour, for free — no separate timer needed since the search is itself bounded
 * to a fixed lookback each frame). Best on bass/kick-heavy or genuinely periodic (oscilloscope-
 * music) material; broadband, transient-heavy passages may still drift between frames.
 *
 * Deliberately does *not* auto-flag clipping (a v1 threshold highlight was tried and dropped): the
 * headroom pre-gain already keeps normal operation away from true digital clipping, streaming
 * loudness normalization mostly erases what "hot" means by the time a listener hears it, and a
 * fixed |sample| threshold can't tell real clipping from a squarish kick transient sitting near
 * full-scale — a false-positive machine, not a useful indicator. Eyeballing flat-topping yourself
 * is still exactly what this view is for; it just shouldn't pretend to automate it.
 *
 * Sits to the left of the vectorscope in the scope chart view — same instrument row, same
 * "loopback monitor" family, sharing its `scope` event stream, its `scope-eq`-driven undistort
 * (inverse-filter cascade), and viewer-count gating with Vectorscope.
 */
export function TimeScope({
  fill = false,
  onPopOut,
}: {
  /** The detached window's view (ScopeWindow): fills it, and leaves counting it as a scope viewer
   *  to the main window, as Vectorscope's `fill` does. */
  fill?: boolean;
  /** Shows a button that detaches the time scope into its own window. */
  onPopOut?: () => void;
} = {}) {
  const { t } = useTranslation();
  const wrapRef = useRef<HTMLDivElement>(null);
  const gridRef = useRef<HTMLCanvasElement>(null);
  const trailRef = useRef<HTMLCanvasElement>(null);
  // The peak-hold lines get their own layer above the trail: they're redrawn at a constant alpha
  // every frame, and the trail accumulates *additively*, so drawing them into it would stack them
  // toward white instead of holding steady (see phosphor.ts's closing note).
  const peakRef = useRef<HTMLCanvasElement>(null);
  // This view's cursor on the scope playout clock (scopeClock.ts): each frame takes the audio played
  // since the last one, all of it into the trigger ring — one continuous stretch, seamless while the
  // stream is.
  const readerRef = useRef<ScopeReader | null>(null);
  const { params, setParams, saveAsDefault, resetToFactory, savedDefault, clearSavedDefault, atSavedDefault } = useTunableParams("cageq-timescope-params", DEFAULTS, TIMESCOPE_PARAMS_MIGRATION);
  const [tuning, setTuning] = useState(false);
  // L/R (two half-height lanes) vs mixdown (one full-height lane, mono sum) — mixdown trades
  // channel separation for 2x the vertical scale, handy when you just want to see how hot/quiet
  // the signal is rather than compare channels. A quick toggle, not a tuning-panel knob — flipped
  // often enough to want single-click access. Inline it's an ordinary `params.mode` field (see
  // Params' own doc), saved/reset alongside everything else; the pop-out keeps its own
  // (POPOUT_MODE_KEY), L/R unless changed there: it has the room the inline tube lacks, which is
  // what mixdown's doubled scale is for. Speed dimming likewise (POPOUT_SPEED_DIM_KEY); everything
  // else is shared with the inline view.
  const [popMode, setPopMode] = useState<Params["mode"]>(() => readStored(POPOUT_MODE_KEY, (v) => (v === "mix" ? "mix" : "lr"), "lr"));
  const [popSpeedDim, setPopSpeedDim] = useState(() =>
    readStored(
      POPOUT_SPEED_DIM_KEY,
      (v) => {
        const x = v === null ? NaN : Number(v);
        return Number.isFinite(x) ? Math.min(1, Math.max(0, x)) : POPOUT_SPEED_DIM_DEFAULT;
      },
      POPOUT_SPEED_DIM_DEFAULT,
    ),
  );
  // What this view draws and its panel shows: the shared params, with the pop-out's own on top.
  const view: Params = fill ? { ...params, mode: popMode, speedDim: popSpeedDim } : params;
  const mode = view.mode;
  const paramsRef = useRef(view);
  paramsRef.current = view;

  // Width tracks the flex row's leftover space (not square, unlike the vectorscope); height tracks
  // `.chart-wrap`'s own `aspect-ratio:720/215` (App.css) via the same wrapper's rendered box, so
  // this instrument's height agrees with the Eq/Impulse views' viewBox-scaled SVGs instead of
  // drifting from a hardcoded value and shifting the layout on every chart-view switch.
  const [width, setWidth] = useState(320);
  const [height, setHeight] = useState(215);
  // The tube's on-screen height, for the render loop's beam width (`beamScale`).
  const tubeRef = useRef(215);
  const fillRef = useRef(fill);
  fillRef.current = fill;
  useEffect(() => {
    const el = wrapRef.current;
    if (!el) return;
    const measure = () => {
      const r = el.getBoundingClientRect();
      setWidth(Math.max(80, Math.floor(r.width)));
      setHeight(Math.max(60, Math.floor(r.height)));
      if (r.height > 0) tubeRef.current = r.height;
      if (!fill && r.height > 0) noteScopeTube(r.height);
    };
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    // Also on a zoom change, which resizes the box on screen without firing the observer (appZoom.ts).
    window.addEventListener(APP_ZOOM_EVENT, measure);
    measure();
    return () => {
      ro.disconnect();
      window.removeEventListener(APP_ZOOM_EVENT, measure);
    };
  }, [fill]);
  const dpr = Math.min(typeof window !== "undefined" ? window.devicePixelRatio || 1 : 1, 2);
  const resW = Math.round(width * dpr);
  const resH = Math.round(height * dpr);

  // The active EQ cascade (for undistort) + the built, crossfading inverse cascade — identical
  // structure to Vectorscope's, independent instance (own running filter state, since this draws
  // its own trace from the same raw stream). `null` until the first EQ arrives.
  const eqRef = useRef<ScopeEq>({ filters: [], preampDb: 0, model: "Rbj" }); // placeholder until the first `scope-eq` arrives
  const invRef = useRef<FadingInverse | null>(null);

  // Own the scope-stream + `scope-eq` subscriptions — independent of the vectorscope's, so either
  // can be open alone. The sample stream is a Channel-backed bus (streams.ts — not `listen`
  // events, whose per-event webview eval churned WebView2's memory at 60 fps); `scope-eq` stays a
  // real event, it only fires on EQ changes. `set_scope_viewer` is a shared count (see
  // Vectorscope/App) gating whether the backend produces scope data at all.
  useEffect(() => {
    let active = true;
    const { reader, release: unsubScope } = scopeReader();
    readerRef.current = reader;
    let unlistenEq: (() => void) | undefined;
    void (async () => {
      unlistenEq = await listen<ScopeEq>("scope-eq", (e) => {
        if (active) eqRef.current = e.payload;
      });
      if (active) void emit("scope-eq-request");
    })();
    // The pop-out's count is the main window's to keep (see Vectorscope's identical rule): this
    // window's own cleanup may not run when its OS window is destroyed.
    if (!fill) void invoke("set_scope_viewer", { active: true });
    return () => {
      active = false;
      unsubScope();
      unlistenEq?.();
      if (!fill) void invoke("set_scope_viewer", { active: false });
    };
  }, [fill]);

  // Static graticule: a centreline per lane (+ the divider between them, in L/R mode). Redrawn on
  // resize or a mode switch (the lane layout itself changes).
  useEffect(() => {
    const cv = gridRef.current;
    const ctx = cv?.getContext("2d");
    if (!cv || !ctx) return;
    const W = cv.width;
    const H = cv.height;
    ctx.clearRect(0, 0, W, H);
    const [ar, ag, ab] = accentRgb(cv);
    ctx.strokeStyle = `rgba(${ar},${ag},${ab},${GRID_ALPHA})`;
    ctx.lineWidth = Math.max(1, H / REF_SIZE);
    ctx.beginPath();
    if (mode === "lr") {
      ctx.moveTo(0, H / 2); // lane divider
      ctx.lineTo(W, H / 2);
    }
    for (const lane of laneLayout(mode, H)) {
      ctx.moveTo(0, lane.cy);
      ctx.lineTo(W, lane.cy);
    }
    ctx.stroke();
    // Time divisions — the classic scope graticule (see ms/div in the tuning panel). Fixed count
    // (DIVISIONS), independent of the actual ms/div value: only the *labelled* span changes with
    // that slider, the grid itself doesn't need to know it. Subtler than the lane centrelines.
    ctx.strokeStyle = `rgba(${ar},${ag},${ab},${DIV_LINE_ALPHA})`;
    ctx.beginPath();
    for (let i = 1; i < DIVISIONS; i++) {
      const x = (i / DIVISIONS) * W;
      ctx.moveTo(x, 0);
      ctx.lineTo(x, H);
    }
    ctx.stroke();
    ctx.fillStyle = `rgba(${ar},${ag},${ab},0.5)`;
    ctx.font = `${Math.round(H * 0.05)}px system-ui, sans-serif`;
    ctx.textBaseline = "middle";
    for (const lane of laneLayout(mode, H)) if (lane.label) ctx.fillText(lane.label, 6, lane.cy - lane.amp * 0.55);
  }, [resW, resH, mode]);

  // The trace: this frame's beam goes onto a scratch canvas handed to the phosphor accumulator,
  // which owns the decay and the additive composite — same machinery as the vectorscope, see
  // phosphor.ts for why the decay isn't an in-place canvas fade any more.
  useEffect(() => {
    const cv = trailRef.current;
    const peakCv = peakRef.current;
    const peakCtx = peakCv?.getContext("2d");
    if (!cv || !peakCv || !peakCtx) return;
    // Linear light: the beam's energy model only reads right if half the energy shows as half the
    // light (see PhosphorOptions).
    const phos = createPhosphor(cv, { linearLight: true });
    if (!phos) return;
    // See SpectrumScope.tsx's identical log — diagnosing a machine-specific "one view reads darker
    // than the others" report by checking whether any view silently fell back off the GPU
    // half-float accumulator (phosphor.ts's `precise`).
    if (!phos.precise) console.warn("[TimeScope] phosphor fell back to the 8-bit canvas accumulator (no half-float GPU support)");
    const [ar, ag, ab] = accentRgb(cv);

    let raf = 0;
    let last = performance.now();
    // The newest trace, redrawn until the next window replaces it (TRACE_HOLD_MS): when it was
    // built, and either the GPU path's segments (in `segData`) or the fallback's paths.
    let heldAt = -Infinity;
    let heldSegs: TraceSegments | null = null;
    let heldPaths: { paths: Path2D[]; width: number; style: string } | null = null;
    // Per-lane peak-hold (dBFS) and the timestamp its release may resume after — indexed to match
    // `lanes` each frame; resized (and reset) on a mode switch, since "lane 0" means something
    // different in L/R vs mixdown. A brief reset on an intentional mode change is unsurprising.
    let peakDb: number[] = [];
    let peakReading: number[] = []; // each lane's latest reading, and when (see PEAK_READING_STALE_MS)
    let peakReadingAt: number[] = [];
    let peakHoldUntil: number[] = [];
    let magScratch = new Float64Array(0); // reused rectified-magnitude buffer for kthLargest
    // Per-scope-window work buffers, reused across windows (grown when a bigger one is needed, never
    // shrunk — every reader is bounded by `n`/`dispN`, never by `.length`, so spare capacity and
    // stale tail contents are harmless). These used to be fresh `new Float64Array`s per window, at
    // the ~60 Hz scope rate: up to ~5 MB/s of typed-array backing stores at the widest ms/div, which
    // V8 accounts as *external* memory — a classic trigger for full mark-compact GCs (the ~20 ms
    // "Major GC" frames in the profiler), unlike small on-heap garbage that scavenges cheaply.
    let outLBuf = new Float64Array(0);
    let outRBuf = new Float64Array(0);
    let dispLBuf = new Float64Array(0);
    // The GPU path's segments, every lane's (see `addSegments`): x0, y0, x1, y1, alpha each. Grown,
    // never shrunk.
    let segData = new Float32Array(0);
    // One lane's samples for the reconstruction, padded by LANCZOS_A each side (the window's own end
    // samples held). Grown, never shrunk.
    let laneVals = new Float32Array(0);
    // The same, before the bandwidth limit (see bwLimit), and its kernel for the sigma it was built
    // for. Grown, never shrunk.
    let laneRaw = new Float32Array(0);
    let laneTmp = new Float32Array(0);
    let bwKernel = new Float32Array(0);
    let bwKernelSigma = -1;
    let dispRBuf = new Float64Array(0);

    // Triggering: a circular buffer of the (possibly undistorted) display samples, plus a parallel
    // buffer of a lowpass-filtered mono trigger-detector value at the same indices — both written
    // incrementally below as data arrives. `totalWritten` is a monotonic sample counter (not a
    // wrapping pointer); physical ring index = `phys(globalIndex)`. See the module-level comment.
    const ringL = new Float64Array(RING_CAP);
    const ringR = new Float64Array(RING_CAP);
    const ringTrig = new Float64Array(RING_CAP);
    let totalWritten = 0;
    const phys = (globalIdx: number) => (((globalIdx % RING_CAP) + RING_CAP) % RING_CAP);
    // The trigger detector's forward state persists across windows (a real filter, not re-zeroed
    // each frame) — rebuilt only when the rate or the tuned cutoff changes, same pattern as `invRef`.
    let trig: TriggerDetector | null = null;
    // The edge drawn last, and `totalWritten` when it was first drawn (see TRIGGER_HOLD_MS).
    let lastEdge = -1;
    let lastEdgeSince = 0;

    const frameDue = frameGate(); // the optional frame-rate cap (frameLimit.ts)
    const render = (ts: number) => {
      // The frame's own time — rAF's timestamp, when the frame began, put on the refresh grid
      // (frameClock.ts) — not when this callback happened to start running: that varies by however
      // long the frame's earlier work took, and the playout clock (scopeClock.ts) ties motion to it.
      const now = frameClock.time(ts);
      if (!frameDue(now)) {
        raf = requestAnimationFrame(render);
        return;
      }
      const dt = Math.max(0, Math.min(0.1, (now - last) / 1000));
      last = now;
      const p = paramsRef.current;
      const mode = p.mode;
      const W = cv.width;
      const H = cv.height;
      const lanes = laneLayout(mode, H);

      if (peakDb.length !== lanes.length) {
        peakDb = lanes.map(() => DB_FLOOR);
        peakHoldUntil = lanes.map(() => 0);
        peakReading = lanes.map(() => DB_FLOOR);
        peakReadingAt = lanes.map(() => -Infinity);
      }
      // Release: continuous decay once past the hold window, every frame regardless of whether new
      // data arrived this frame (so it doesn't step, it eases down smoothly between scope emits).
      // Down to the latest reading, not past it (see PEAK_READING_STALE_MS).
      for (let lane = 0; lane < lanes.length; lane++) {
        if (now >= peakHoldUntil[lane]) {
          const floor = now - peakReadingAt[lane] < PEAK_READING_STALE_MS ? peakReading[lane] : DB_FLOOR;
          peakDb[lane] = Math.max(peakDb[lane] - PEAK_RELEASE_DB_PER_SEC * dt, floor);
        }
      }

      const take = readerRef.current?.take(now) ?? { slices: [], state: "idle" as const, rate: 48000 };
      if (take.state === "idle") heldAt = -Infinity; // gone quiet: no trace to hold
      const fresh = take.slices;
      const isNew = fresh.length > 0;
      // Computed up front (before drawing) when new data arrives, so the peak-hold attack below and
      // the trace path further down both read the identical (possibly undistorted) samples.
      let outL: Float64Array | null = null;
      let outR: Float64Array | null = null;
      // The window actually drawn — free-run: the newest ms/div × DIVISIONS span; triggered:
      // anchored at the most recent qualifying edge found in the ring buffer. Set below, after
      // outL/outR exist.
      let dispL: Float64Array | null = null;
      let dispR: Float64Array | null = null;
      let dispN = 0;
      // A trace on every frame (see TRACE_HOLD_MS), so a frame's worth of dose (`frameDoseMult`),
      // with the Trail/Glow orthogonality factor (TAU_REF/p.trailTau — see TAU_REF's own doc).
      const doseMult = frameDoseMult(p.trailTau, TAU_REF);
      if (isNew) {
        let rate = 48000;
        for (const s of fresh) {
          const n = s.n;
          const { l: sl, r: sr, start } = s;

          // Undistort: (re)build the inverse cascade when the filters/rate change or the mode turns
          // on, then run each sample back through it — same machinery/rationale as Vectorscope's.
          // Retargeting crossfades from whatever was running rather than snapping (`FadingInverse`
          // in biquad.ts): an instant swap applies the *new* inverse to samples that are still the
          // *old* filter's real output, a genuine discontinuity a snapshot recording proved isn't
          // in the real (crossfaded) audio at all — see `apo-switch-artifacts` memory, "NOT a bug".
          const eq = eqRef.current;
          rate = s.rate && s.rate > 0 ? s.rate : 48000;
          if (p.undistort) {
            const iv = invRef.current;
            if (iv === null || iv.filtersRef !== eq.filters || iv.model !== eq.model || iv.rate !== rate) {
              invRef.current = retargetFadingInverse(iv, eq.filters, eq.preampDb, eq.model, rate);
            }
          } else {
            invRef.current = null; // dropped while off; turning back on starts a fresh cascade, no crossfade to a mode that wasn't running
          }
          const iv = invRef.current;
          // Runs with an empty cascade too when there's just a preamp to undo (e.g. Dry, which
          // carries the loudness-match gain but no EQ) — pure gain recovery, no filtering —
          // or while fading out a departing cascade that had either.
          const undistort = iv !== null && (iv.to.coeffs.length > 0 || iv.to.gain !== 1 || iv.from !== null);
          if (outLBuf.length < n) {
            outLBuf = new Float64Array(n);
            outRBuf = new Float64Array(n);
          }
          outL = outLBuf;
          outR = outRBuf;
          for (let i = 0; i < n; i++) {
            let l = sl[start + i];
            let r = sr[start + i];
            if (undistort) {
              [l, r] = stepFadingInverse(iv!, l, r);
            }
            outL[i] = l;
            outR[i] = r;
          }

          // Feed the ring buffer: raw display samples plus a lowpass-filtered mono trigger-detector
          // value, one sample at a time so the filter's state stays continuous across windows (no
          // re-filtering, no transient reset). Rebuild the filter only when rate/cutoff changed, and
          // then run the reachable history through it again (TRIGGER_REFILTER_MS).
          if (!trig || trig.rate !== rate || trig.hz !== p.triggerFilterHz) {
            trig = triggerDetector(rate, p.triggerFilterHz);
            const from = totalWritten - Math.min(totalWritten, RING_CAP, Math.round((rate * TRIGGER_REFILTER_MS) / 1000));
            for (let i = from; i < totalWritten; i++) {
              const idx = phys(i);
              ringTrig[idx] = detectorSample(trig, (ringL[idx] + ringR[idx]) / 2);
            }
          }
          for (let i = 0; i < n; i++) {
            const idx = phys(totalWritten);
            ringL[idx] = outL[i];
            ringR[idx] = outR[i];
            ringTrig[idx] = detectorSample(trig, (outL[i] + outR[i]) / 2);
            totalWritten++;
          }

          // Attack: the peak per lane up to the end of this window, from the exact values the trace
          // draws (the ring holds them, undistorted or not) — correct in both modes by construction.
          // The rectified signal's K-th largest sample (PEAK_CONFIRM_MS) over the last PEAK_SPAN_MS.
          const confirmSamples = Math.max(1, Math.round(rate * (PEAK_CONFIRM_MS / 1000)));
          const span = Math.min(totalWritten, RING_CAP, Math.max(1, Math.round(rate * (PEAK_SPAN_MS / 1000))));
          if (magScratch.length < span) magScratch = new Float64Array(span);
          for (let lane = 0; lane < lanes.length; lane++) {
            for (let j = 0; j < span; j++) {
              const idx = phys(totalWritten - span + j);
              magScratch[j] = Math.abs(mode === "mix" ? (ringL[idx] + ringR[idx]) / 2 : lane === 0 ? ringL[idx] : ringR[idx]);
            }
            const blockPeak = kthLargest(magScratch, span, confirmSamples);
            const blockDb = 20 * Math.log10(Math.max(blockPeak, 1e-6));
            peakReading[lane] = blockDb;
            peakReadingAt[lane] = now;
            if (blockDb >= peakDb[lane]) {
              peakDb[lane] = blockDb;
              peakHoldUntil[lane] = now + PEAK_HOLD_MS;
            }
          }
        }

        // Pick the display window: free-run always anchors at the newest valid position; triggered
        // searches backward (bounded to TRIGGER_SEARCH_MS) for the most recent rising edge in the
        // trigger signal (see findRisingEdge), falling back to the free-run anchor if none qualifies —
        // that fallback *is* the auto-trigger/timeout behaviour, no separate timer needed since the
        // search window itself is bounded fresh every frame.
        const msPerDiv = MS_PER_DIV_STEPS[p.msPerDivIdx] ?? MS_PER_DIV_STEPS[0];
        const windowSamples = Math.max(2, Math.round(rate * ((msPerDiv * DIVISIONS) / 1000)));
        const preSamples = Math.round((windowSamples * TRIGGER_PRE_DIVS) / DIVISIONS);
        const postSamples = windowSamples - preSamples;
        const ringCount = Math.min(totalWritten, RING_CAP);
        if (ringCount >= windowSamples) {
          const tMax = totalWritten - postSamples; // newest position with enough post-trigger data
          let t = tMax;
          if (p.trigger && trig) {
            const searchSamples = Math.round(rate * (TRIGGER_SEARCH_MS / 1000));
            const tMin = Math.max(totalWritten - ringCount + preSamples, tMax - searchSamples);
            const edge = findRisingEdge(trig, ringTrig, phys, totalWritten - ringCount, totalWritten - 1, tMin, tMax);
            if (edge !== null && edge !== lastEdge) {
              lastEdge = edge;
              lastEdgeSince = totalWritten;
            }
            // The same edge for too long: free-run (tMax) until a newer one turns up.
            if (edge !== null && totalWritten - lastEdgeSince <= (rate * TRIGGER_HOLD_MS) / 1000) t = edge;
          }
          dispN = windowSamples;
          if (dispLBuf.length < windowSamples) {
            dispLBuf = new Float64Array(windowSamples);
            dispRBuf = new Float64Array(windowSamples);
          }
          dispL = dispLBuf;
          dispR = dispRBuf;
          for (let j = 0; j < windowSamples; j++) {
            const idx = phys(t - preSamples + j);
            dispL[j] = ringL[idx];
            dispR[j] = ringR[idx];
          }
        }
      }

      // 1) Start this frame's trace on a cleared scratch canvas; decaying everything before it is
      // the accumulator's job, applied in commit() below. On the GPU path (`addSegments`) there's
      // no canvas to start: a frame that draws nothing leaves an empty trace either way.
      const ctx = phos.addSegments ? null : phos.begin();

      // 2) Faint peak-hold line(s), mirrored ± around each lane's centreline (a waveform is
      // bipolar; the peak tracked above is a magnitude). On their own cleared layer rather than in
      // the trail: they're redrawn at a constant alpha every frame and the trail is additive, so
      // accumulating them would stack them toward white. The layer sits *below* the trail (DOM
      // order — see the canvases below), preserving the original intent that the beam reads over
      // the reference line wherever they cross, rather than the line painting across the beam.
      peakCtx.clearRect(0, 0, W, H);
      peakCtx.strokeStyle = `rgba(230,162,60,${PEAK_LINE_ALPHA})`; // #e6a23c — same amber as .vbar-peak
      peakCtx.lineWidth = Math.max(1, H / REF_SIZE);
      peakCtx.beginPath();
      for (let lane = 0; lane < lanes.length; lane++) {
        const { cy, amp } = lanes[lane];
        const dy = Math.pow(10, peakDb[lane] / 20) * amp;
        // Under a pixel from the centreline is no peak to show: drawn there, at startup and after
        // a silence has released it, the pair read as a reference line on every lane.
        if (dy < peakCtx.lineWidth) continue;
        peakCtx.moveTo(0, cy - dy);
        peakCtx.lineTo(W, cy - dy);
        peakCtx.moveTo(0, cy + dy);
        peakCtx.lineTo(W, cy + dy);
      }
      peakCtx.stroke();

      // 3) The trace itself — the extracted (free-run or triggered) display window, not the raw
      // per-event samples: the window is a fixed ms/div × DIVISIONS span, decoupled from the
      // backend's emission size, and (when triggered) anchored at the found edge rather than
      // "whatever just arrived".
      if (isNew && dispL && dispR && dispN >= 2) {
        // L/R: one path per channel/lane. Mixdown: one path, mono sum, the single full-height lane.
        const valueAt = (i: number, lane: number): number => (mode === "mix" ? (dispL![i] + dispR![i]) / 2 : lane === 0 ? dispL![i] : dispR![i]);
        // The width it has inline at the design size, however much bigger the tube is on screen —
        // a maximised window, or the detached one (see `beamScale`).
        const lineWidth = Math.max(0.6, p.beam * (H / REF_SIZE) * beamScale(fillRef.current, tubeRef.current));
        if (ctx) {
          const paths = lanes.map(() => new Path2D());
          for (let lane = 0; lane < lanes.length; lane++) {
            const { cy, amp } = lanes[lane];
            const path = paths[lane];
            path.moveTo(0, cy - valueAt(0, lane) * amp);
            for (let i = 1; i < dispN; i++) path.lineTo((i / (dispN - 1)) * W, cy - valueAt(i, lane) * amp);
          }
          ctx.globalCompositeOperation = "lighter";
          ctx.lineWidth = lineWidth;
          ctx.lineJoin = "round";
          ctx.lineCap = "round";
          ctx.strokeStyle = `rgba(${ar},${ag},${ab},${p.glow * FALLBACK_GLOW_SCALE})`;
          for (const path of paths) ctx.stroke(path);
          heldPaths = { paths, width: lineWidth, style: ctx.strokeStyle };
        } else {
          // The beam, as a CRT's (like Vectorscope's and the meter's): a fixed current, so each
          // sample interval deposits the same energy, spread over however far the beam travels in
          // it — a flat stretch, where it only moves with the sweep, is bright, and a steep edge,
          // where it jumps a long way in one interval, is faint (a square wave's edges nearly
          // vanish, as on a real scope). Energy per sample = glow x the sweep's step per sample, so
          // a flat stretch reads exactly glow at any timebase, as the plain stroke did. Not 1/sweep
          // speed, as a real scope's would: this redraws one window per data update whatever the
          // timebase, so sweeps per second aren't physical here to begin with. Additive across the
          // lanes and wherever the trace crosses itself. The path between samples is reconstructed
          // (see PIECE_PX), and the spot is round (see TraceSegments' `round`): together, a smooth
          // curve reads as one, with no dot at the joins.
          //
          // Optionally band-limited first (bwLimit), a real scope's "BW limit" switch, with its
          // cutoff set in screen pixels rather than Hz: a Gaussian low-pass, sigma = bwLimit px of
          // sweep. Gaussian because its step response has no overshoot — a square wave keeps flat
          // tops, its edges round over ~2.5 sigma — where a steeper filter would ring (Gibbs). In
          // pixels because what it's for is detail finer than the timebase can show as shape: at a
          // long timebase dense content has many samples per pixel, and what's finer than a few
          // pixels across only shows as vertical grass and speckle. At short timebases sigma falls
          // well under a sample, and it does nothing (skipped under BW_MIN_SIGMA). The cost: high-
          // frequency content shows less of its height there (a 10 kHz tone at 5 ms/div shrinks
          // toward a line), which is why it's a setting.
          //
          // How much a fast stretch dims is a setting too (speedDim): brightness per length goes as
          // (sweep step / length)^speedDim, so 1 is the beam above and 0 a drawn line, every part of
          // the path as bright as a flat stretch, as before the beam was simulated. In between, fast
          // stretches dim less. Not physics but readability: on dense content the real beam leaves
          // nearly all its light at the turning points of each wiggle, a speckle that lands somewhere
          // new every frame and reads as a fuzzy, shimmering band rather than a waveform; the drawn
          // line keeps each trace one continuous stroke. A flat stretch reads glow either way.
          const step = W / (dispN - 1);
          const maxPieces = Math.min(MAX_PIECES, Math.max(DENSE_PIECES, Math.ceil(step)));
          const padded = dispN + LANCZOS_TAPS;
          if (laneVals.length < padded) {
            laneVals = new Float32Array(padded);
            laneRaw = new Float32Array(padded);
            laneTmp = new Float32Array(padded);
          }
          // The bandwidth limit's sigma, in samples, and its kernel (rebuilt when that changes).
          const bwSigma = p.bwLimit / step;
          const bwOn = bwSigma >= BW_MIN_SIGMA;
          const bwBoxes = bwOn && bwSigma > BW_DIRECT_MAX_SIGMA ? boxesForGauss(bwSigma, 3) : null;
          if (bwOn && !bwBoxes && bwSigma !== bwKernelSigma) {
            const r = Math.ceil(3 * bwSigma);
            bwKernel = new Float32Array(2 * r + 1);
            let sum = 0;
            for (let j = -r; j <= r; j++) sum += bwKernel[j + r] = Math.exp((-0.5 * j * j) / (bwSigma * bwSigma));
            for (let j = 0; j < bwKernel.length; j++) bwKernel[j] /= sum;
            bwKernelSigma = bwSigma;
          }
          let o = 0;
          for (let lane = 0; lane < lanes.length; lane++) {
            const { cy, amp } = lanes[lane];
            const src = bwOn ? laneRaw : laneVals;
            for (let k = 0; k < padded; k++) src[k] = valueAt(Math.min(dispN - 1, Math.max(0, k - LANCZOS_A)), lane);
            if (bwBoxes) {
              boxBlur(laneRaw, laneTmp, padded, bwBoxes[0]);
              boxBlur(laneTmp, laneRaw, padded, bwBoxes[1]);
              boxBlur(laneRaw, laneVals, padded, bwBoxes[2]);
            } else if (bwOn) {
              // The window's ends held beyond it, as the padding already is.
              const r = (bwKernel.length - 1) / 2;
              for (let k = 0; k < padded; k++) {
                let v = 0;
                for (let j = -r; j <= r; j++) v += bwKernel[j + r] * laneRaw[Math.min(padded - 1, Math.max(0, k + j))];
                laneVals[k] = v;
              }
            }
            let x0 = 0;
            let y0 = cy - laneVals[LANCZOS_A] * amp;
            for (let i = 0; i < dispN - 1; i++) {
              // Pieces by the chord's on-screen length (the reconstructed path is close to it).
              const yEnd = cy - laneVals[i + 1 + LANCZOS_A] * amp;
              const n = Math.min(maxPieces, Math.max(1, Math.ceil(Math.hypot(step, yEnd - y0) / PIECE_PX)));
              if (o + 5 * n > segData.length) {
                const grown = new Float32Array(Math.max(2 * segData.length, o + 5 * n, 5 * lanes.length * (dispN - 1)));
                grown.set(segData.subarray(0, o));
                segData = grown;
              }
              const pieceDx = step / n;
              for (let j = 1; j <= n; j++) {
                let y1 = yEnd;
                if (j < n) y1 = cy - lanczosAt(laneVals, i, lanczosRow(j / n)) * amp;
                const x1 = (i + j / n) * step;
                segData[o] = x0;
                segData[o + 1] = y0;
                segData[o + 2] = x1;
                segData[o + 3] = y1;
                const len = Math.hypot(x1 - x0, y1 - y0); // never zero: x always moves
                segData[o + 4] = p.glow * (p.speedDim === 1 ? pieceDx / len : Math.pow(pieceDx / len, p.speedDim));
                o += 5;
                x0 = x1;
                y0 = y1;
              }
            }
          }
          heldSegs = { data: segData, count: o / 5, width: lineWidth, rgb: [ar, ag, ab], round: true };
          phos.addSegments!(heldSegs);
        }
        heldAt = now;
      } else if (take.state === "starved" && now - heldAt < TRACE_HOLD_MS) {
        // No audio for this frame yet (a late window): the newest trace again (see TRACE_HOLD_MS).
        if (ctx && heldPaths) {
          ctx.globalCompositeOperation = "lighter";
          ctx.lineWidth = heldPaths.width;
          ctx.lineJoin = "round";
          ctx.lineCap = "round";
          ctx.strokeStyle = heldPaths.style;
          for (const path of heldPaths.paths) ctx.stroke(path);
        } else if (!ctx && heldSegs) {
          phos.addSegments!({ ...heldSegs, reuse: true });
        }
      }

      // 4) Hand the frame's trace to the accumulator — it decays the history and adds this on top.
      phos.setTone?.(p.saturation, p.contrast, p.headroom, p.bloomWhite);
      phos.commit(dt, p.trailTau, p.tail, doseMult, p.bloom, p.haze);

      raf = requestAnimationFrame(render);
    };
    raf = requestAnimationFrame(render);
    return () => {
      cancelAnimationFrame(raf);
      phos.dispose(); // frees the GL textures/programs; a leaked context would survive the unmount
    };
  }, []);

  const set = <K extends keyof Params>(k: K, v: Params[K]) => setParams((prev) => ({ ...prev, [k]: v }));
  const setPopModeStored = (next: Params["mode"]) => {
    setPopMode(next);
    writeStored(POPOUT_MODE_KEY, next);
  };
  const setPopSpeedDimStored = (next: number) => {
    setPopSpeedDim(next);
    writeStored(POPOUT_SPEED_DIM_KEY, String(next));
  };
  const toggleMode = () => {
    const next = mode === "lr" ? "mix" : "lr";
    if (fill) setPopModeStored(next);
    else set("mode", next);
  };
  const setNum = (k: NumKey, v: number) => {
    if (fill && k === "speedDim") setPopSpeedDimStored(v);
    else set(k, v);
  };
  // In the pop-out, Reset also puts its own settings back to theirs.
  const reset = () => {
    resetToFactory();
    if (fill) {
      setPopModeStored("lr");
      setPopSpeedDimStored(POPOUT_SPEED_DIM_DEFAULT);
    }
  };
  type NumKey = "trailTau" | "tail" | "glow" | "beam" | "bloom" | "haze" | "saturation" | "contrast" | "headroom" | "bloomWhite" | "bwLimit" | "speedDim" | "triggerFilterHz";
  const CONTROLS: { key: NumKey; label: string; hint: string; min: number; max: number; step: number }[] = [
    { key: "trailTau", label: t("scope.trail"), hint: t("scope.trailHint"), min: 0.02, max: 2.0, step: 0.01 },
    { key: "tail", label: t("scope.tail"), hint: t("scope.tailHint"), min: 1, max: 64, step: 1 },
    { key: "glow", label: t("scope.glow"), hint: t("scope.glowHint"), min: 0.02, max: 12, step: 0.02 },
    { key: "beam", label: t("scope.beam"), hint: t("scope.beamHint"), min: 0.1, max: 8, step: 0.05 },
    { key: "bloom", label: t("scope.bloom"), hint: t("scope.bloomHint"), min: 0, max: 2, step: 0.05 },
    { key: "haze", label: t("scope.haze"), hint: t("scope.hazeHint"), min: 0, max: 4, step: 0.05 },
    { key: "saturation", label: t("scope.saturation"), hint: t("scope.saturationHint"), min: 0, max: 10, step: 0.1 },
    { key: "contrast", label: t("scope.contrast"), hint: t("scope.contrastHint"), min: 1, max: 3, step: 0.05 },
    { key: "headroom", label: t("scope.headroom"), hint: t("scope.headroomHint"), min: 0.5, max: 16, step: 0.1 },
    { key: "bloomWhite", label: t("scope.bloomWhite"), hint: t("scope.bloomWhiteHint"), min: 0, max: 1, step: 0.05 },
    { key: "bwLimit", label: t("scope.bwLimit"), hint: t("scope.bwLimitHint"), min: 0, max: 3, step: 0.1 },
    { key: "speedDim", label: t("scope.speedDim"), hint: t("scope.speedDimHint"), min: 0, max: 1, step: 0.05 },
    { key: "triggerFilterHz", label: t("scope.trigFilter"), hint: t("scope.trigFilterHint"), min: 20, max: 1000, step: 10 },
  ];
  const msPerDiv = MS_PER_DIV_STEPS[params.msPerDivIdx] ?? MS_PER_DIV_STEPS[0];

  return (
    <div className={`timescope-wrap${fill ? " fill" : ""}`} ref={wrapRef}>
      {/* width/height here are CSS "100%" (matching the wrap exactly, sub-pixel precise) — the
          JS-measured `width`/`resW`/`resH` state feeds only the canvas backing-store *resolution*
          attributes below, never the display size. A JS-measured, floored pixel value here would
          drift by up to 1px from the wrap's true (CSS-computed) height and show up as exactly the
          kind of small persistent misalignment against EqChart's own height:auto SVG sizing. */}
      <div className="vs-screen" style={{ width: "100%", height: "100%" }}>
        <canvas
          ref={gridRef}
          className="vectorscope-canvas vs-grid"
          width={resW}
          height={resH}
          style={{ width: "100%", height: "100%" }}
          aria-hidden="true"
        />
        <canvas
          ref={peakRef}
          className="vectorscope-canvas vs-peak"
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
        <div className="vs-tools">
          {onPopOut && (
            <button type="button" className="vs-tool" title={t("scope.popOut")} onClick={onPopOut}>
              ⤢
            </button>
          )}
          <button
            type="button"
            className="vs-tool vs-tool-text"
            title={mode === "lr" ? t("scope.toMixTitle") : t("scope.toLrTitle")}
            onClick={toggleMode}
          >
            {mode === "lr" ? t("scope.modeLr") : t("scope.modeMix")}
          </button>
          <button
            type="button"
            className={`vs-tool vs-tool-text${params.trigger ? " on" : ""}`}
            title={params.trigger ? t("scope.toFreeTitle") : t("scope.toTrigTitle")}
            aria-pressed={params.trigger}
            onClick={() => set("trigger", !params.trigger)}
          >
            {params.trigger ? t("scope.modeTrig") : t("scope.modeFree")}
          </button>
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
              {/* While the Saved preset is showing, saving again would change nothing: the button
                  offers to delete it instead (see useTunableParams). */}
              <button
                type="button"
                className="vs-tune-reset"
                title={atSavedDefault ? t("scope.deleteDefaultHint") : undefined}
                onClick={atSavedDefault ? clearSavedDefault : saveAsDefault}
              >
                {t(atSavedDefault ? "scope.deleteDefault" : "scope.saveDefault")}
              </button>
              <button type="button" className="vs-tune-reset" onClick={reset}>
                {t("scope.reset")}
              </button>
              <button type="button" className="vs-tune-close" title={t("scope.close")} aria-label={t("scope.close")} onClick={() => setTuning(false)}>
                ×
              </button>
            </div>
            <label className="vs-tune-row" title={t("scope.msPerDivHint", { total: (msPerDiv * DIVISIONS).toFixed(1) })}>
              <span className="vs-tune-label">{t("scope.msPerDiv")}</span>
              <input
                name="ms-per-div"
                type="range"
                min={0}
                max={MS_PER_DIV_STEPS.length - 1}
                step={1}
                value={params.msPerDivIdx}
                onChange={(e) => set("msPerDivIdx", Number(e.currentTarget.value))}
              />
              <b>{msPerDiv < 1 ? msPerDiv.toFixed(1) : msPerDiv}</b>
            </label>
            <div className="vs-tune-row vs-tune-check" title={t("scope.presetHintTime")}>
              <span className="vs-tune-label">{t("scope.preset")}</span>
              <div className="pl-toggle vs-tune-seg">
                {(Object.keys(PRESETS) as (keyof typeof PRESETS)[]).map((key) => {
                  const pr = PRESETS[key];
                  const on = params.trailTau === pr.trailTau && params.tail === pr.tail && params.glow === pr.glow;
                  return (
                    <button key={key} type="button" className={on ? "on" : ""} onClick={() => setParams((prev) => ({ ...prev, ...pr }))}>
                      {t(PRESET_LABELS[key])}
                    </button>
                  );
                })}
                {savedDefault && (
                  <button
                    type="button"
                    className={atSavedDefault ? "on" : ""}
                    title={t("scope.presetSavedHint")}
                    onClick={() => setParams(savedDefault)}
                  >
                    {t("scope.presetSaved")}
                  </button>
                )}
              </div>
            </div>
            {CONTROLS.map((cc) => (
              <label key={cc.key} className="vs-tune-row" title={cc.hint}>
                <span className="vs-tune-label">{cc.label}</span>
                <input
                  name={`scope-${cc.key}`}
                  type="range"
                  min={cc.min}
                  max={cc.max}
                  step={cc.step}
                  value={view[cc.key]}
                  onChange={(e) => setNum(cc.key, Number(e.currentTarget.value))}
                />
                <b>{view[cc.key].toFixed(cc.step >= 1 ? 0 : cc.step >= 0.1 ? 1 : cc.step >= 0.01 ? 2 : 3)}</b>
              </label>
            ))}
            <div className="vs-tune-sep" />
            <label className="vs-tune-row vs-tune-check" title={t("scope.undistortHint")}>
              <span className="vs-tune-label">{t("scope.undistort")}</span>
              <input name="scope-undistort" type="checkbox" checked={params.undistort} onChange={(e) => set("undistort", e.currentTarget.checked)} />
            </label>
            <FrameCapRow />
          </div>
        )}
      </div>
    </div>
  );
}
