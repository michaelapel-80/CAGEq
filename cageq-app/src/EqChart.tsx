import { useEffect, useId, useMemo, useRef, useState, type ReactNode, type RefObject } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import { Band, composedCurveDb, logGrid, phaseDeg } from "./biquad";
import { createPhosphor, DOSE_REF_FPS } from "./phosphor";
import { traceSmooth } from "./spline";
import { useTunableParams } from "./useTunableParams";

/**
 * §5.2 interactive diagram.
 *
 * Log frequency axis (20 Hz – 20 kHz, constant ratio per octave, the audio-standard
 * view) against dB. Curves are computed client-side from band lists with AutoEq's
 * exact biquad model (see biquad.ts), so they match the fit and what EqAPO applies —
 * and so drag interactions can recompute locally without an IPC round-trip.
 *
 * Takes a list of series (curves) plus optional markers (fixed band handles, e.g. the
 * AutoEq fit shown as diamonds) so it can overlay the editable tone layer, the applied
 * total, and the other comparison slots (Dry/A/B) at once. Any series or marker set can
 * be toggled from the legend — a click hides it, so a busy overlay stays legible.
 */

export type Series = {
  /** Stable identity for the legend toggle and React keys. */
  id: string;
  bands: Band[];
  color: string;
  label: string;
  /** Dimmed, thinner context line (not the layer being edited / not the active slot). */
  muted?: boolean;
  /** Start hidden (user can reveal via the legend) — for opt-in "nerd" layers. */
  defaultHidden?: boolean;
  /** Still drawn (and still clipped to whatever range results), but its values don't count toward
   *  the Y auto-range — for a curve whose *shape* is meaningful but whose *magnitude* isn't
   *  representative of "the applied filter": §5.2 isolate substitutes a bandpass with deep
   *  attenuation outside the audition band for exactly this series' bands, and letting that drive
   *  the scale blows the axis out to reflect the audition, not the actual correction, at exactly
   *  the moment a user is most likely to glance at the chart without re-reading the axis. */
  excludeFromScale?: boolean;
};

/** A fixed (non-draggable) set of band handles drawn as diamonds — e.g. the AutoEq fit. */
export type Marker = {
  id: string;
  bands: Band[];
  color: string;
  label: string;
  defaultHidden?: boolean;
};

/**
 * A pre-sampled reference curve given as raw (frequency, dB) points rather than bands —
 * e.g. AutoEq's ideal correction, which isn't a biquad cascade. Drawn as a thin dotted
 * line so it reads as context the fitted curve is chasing, not another EQ layer.
 */
export type RefCurve = {
  id: string;
  points: { f: number; db: number }[];
  color: string;
  label: string;
  defaultHidden?: boolean;
};

/** The filter chain's phase response, drawn on a **secondary right axis** (degrees) — a
 *  Bode-style overlay, off by default. Computed from bands, so it tracks live edits. */
export type PhaseCurve = { id: string; bands: Band[]; color: string; label: string; defaultHidden?: boolean };

/** A live loopback-FFT spectrum snapshot — log-frequency magnitude bins, in dB (relative), plus
 *  the endpoint's silence flag (`signal`, same test as the meter's): during silence the backend
 *  sweeps `db` down to the floor, and SpectrumScope blanks its beam on `signal` rather than
 *  painting that sweep into its phosphor trail (this chart's own backdrop keeps drawing it — the
 *  gentle fade-away is the intended look here). The capture is the post-EQ output; the chart draws
 *  the **pre-filter** (source) view by removing the applied filter response per bin (see
 *  `eqBands`), so it reads against the EQ curve as "what's coming in" rather than the
 *  already-corrected output (§5.3c). */
export type SpectrumData = { db: number[]; signal: boolean; f_min: number; f_max: number };

/** Draggable band handles: X = centre frequency, Y = gain, wheel = Q (§5.2). */
export type Nodes = {
  bands: Band[];
  color: string;
  onChange: (index: number, patch: Partial<Band>) => void;
  /** Fired once when a drag finishes (for a final, un-throttled commit). */
  onDragEnd?: () => void;
  /** Double-click empty plot space to create a band there (x → fc, y → gain). */
  onAdd?: (freq_hz: number, gain_db: number) => void;
  /** Double-click a node to remove that band (the symmetric gesture to onAdd). */
  onRemove?: (index: number) => void;
  /** Index of a just-added band to pulse-highlight so the eye catches it. */
  highlightIdx?: number;
  /** Externally highlighted band — the pointer is over its column in the grid, so draw the
   *  node active (enlarged + readout) even though the chart itself isn't being hovered. */
  hoverIdx?: number | null;
  /** Reports which band the chart pointer is over (or dragging) so the grid can echo the
   *  highlight; null when the pointer leaves the handles. */
  onHover?: (index: number | null) => void;
  disabled?: boolean;
};

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));

/** Parse a `#rrggbb` hex (e.g. the `--accent` CSS var) to [r,g,b]; null if not a 6-digit hex. */
function parseHex(hex: string): [number, number, number] | null {
  const m = hex.trim().match(/^#?([0-9a-f]{6})$/i);
  if (!m) return null;
  const n = parseInt(m[1], 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

// --- FFT spectrum backdrop: its own tuning set, independent of the level meter's phosphor ---
const SPEC_TOP_DB = 0; // top of the fixed dBFS scale
const SPEC_DYN = 90; // dB shown below the top
// Reference plot height the top-edge stroke's own lineWidth is scaled against — matches the
// component's default `height` prop, same idea as the scope views' REF_SIZE (their own canvases
// just have far more range in practical size than this chart's `height` prop typically does).
const SPEC_STROKE_REF_H = 210;
// Time constant a light exponential smoothing filter applies to the stroke's own Y values only (not
// the wash's) before drawing — see the render effect's own comment for why. Deliberately much
// shorter than the wash's own `tau`: this exists purely to damp small, fast frame-to-frame wobble,
// not to look like a trail — cageq-monitor's own SPEC_TAU_SECS (0.02s) already smooths the backend
// data lightly for responsiveness, and this adds just a bit more on top for the eye specifically.
const STROKE_SMOOTH_TAU = 0.05;
// Flat (not gradient) alpha for the ambient wash fill under the curve, as a fraction of the live
// `glowBase` — see the backdrop effect's own comment for why this stopped being a position-graded
// gradient at all. A fraction of `glowBase` rather than its own fixed constant: the panel only
// exposes one Glow knob, and a wash pinned to an independent fixed value can't be rebalanced
// against whatever the stroke (below) is dialed to — it either overpowers a low stroke setting or
// disappears under a high one. Coupling them keeps their relative balance fixed at this ratio while
// the one slider scales both together. Bumped from an initial 0.2, reported live as still reading
// flat/dim even with glowBase maxed — plain `add` at this ratio never got close to the 1.0 clamp
// (confirming the self-limiting `over` blend really was unnecessary, not just less necessary), so
// there was real headroom to push the wash brighter without any risk of blowout.
const SPEC_WASH_FRAC = 0.5;
// The stroke deliberately does NOT use the wash's tinted-toward-neutral [r,g,b] (see SPEC_NEUTRAL
// below) — it uses the raw theme accent directly. First tried sharing the wash's colour with only a
// higher alpha (an explicit SPEC_STROKE_BOOST multiplier) — reported live as looking more opaque but
// never actually brighter, at any boost value, which is the correct outcome for the wrong fix: alpha
// only controls how much of a colour shows over the canvas underneath, it can never make that colour
// itself lighter than its own RGB values. The wash only reads as bright, saturated, near-white
// because *many frames* of additive accumulation keep summing that dim base colour toward the 1.0
// clamp — a single-pass stroke has no such stacking to lean on, so it needs to start from a colour
// that's already bright on its own, not a dim one boosted by opacity alone.
// The glow's neutral base before tinting toward the accent — fixed, not the theme's own text
// colour (`--fg`, which is near-black in light mode and near-white in dark mode). Tinting a
// near-white base toward the accent in dark mode, then compositing that over a dark backdrop
// through many overlapping stamped layers, overshoots badly (light-on-dark alpha blending
// accumulates brighter than dark-on-light in gamma-encoded sRGB space) — a flat SPEC_DARK_SCALE
// multiplier tried to compensate for exactly this, but needed constant re-tuning as the renderer's
// own layering changed, and the result is a wash from white toward the accent instead of the
// intended dim colour hint. Just always using the same (light-theme) base sidesteps the asymmetry
// instead of correcting for it — same tone, same visual weight in both themes.
const SPEC_NEUTRAL: [number, number, number] = [15, 15, 15]; // light mode's --fg (#0f0f0f)
const SPEC_TINT = 0.63; // blend the accent this far into the neutral glow — a hint of colour, not a rival to the curves
// Reference cadence the wash fill's own alpha (glowBase * SPEC_WASH_FRAC) is calibrated against — matches
// cageq-monitor's SPECTRUM_INTERVAL (~60 Hz). The backdrop effect below redraws every animation
// frame rather than only when a new payload lands (fixed a `punch`-exposed flicker — see its own
// comment), which injects the fill's ink far more often than this was tuned against on any display
// faster than SPEC_UPDATE_HZ — corrected via commit()'s `doseMult` (a further multiplier on its own
// dose math, in the shader's float precision — deliberately NOT a `ctx.globalAlpha` scale on the
// fill itself: that's an 8-bit canvas op, and a low globalAlpha on a smooth gradient visibly
// dithered in this WebView2 build, Chromium/Skia's own anti-banding dither made visible by pushing
// an already-smooth low-alpha gradient into a handful of 8-bit levels).
//
// `doseMult` must be `SPEC_UPDATE_HZ / DOSE_REF_FPS` — a plain constant, NOT scaled by `dt` again.
// First attempt used `dt * SPEC_UPDATE_HZ` and reintroduced refresh-rate-dependent brightness, just
// inverted (brighter at LOW Hz this time): commit()'s own `dt/DOSE_REF_DT` already fully corrects
// for render rate on its own (that's its entire purpose), so multiplying in a SECOND `dt`-dependent
// term made the total dose scale as dt² instead of being rate-independent — overcorrecting at high
// Hz, undercorrecting at low Hz. The fix restores exactly one power of `dt` (the one `uDose` already
// contributes): with a fixed `doseMult`, total dose-per-second at any display rate F works out to
// F × glowBase × (dt/DOSE_REF_DT) × doseMult, and since dt ≈ 1/F this reduces to a constant
// (glowBase × doseMult / DOSE_REF_DT) with no F left in it at all — confirmed algebraically at
// 60/120/240 Hz before landing, not just eyeballed on one machine.
const SPEC_UPDATE_HZ = 60;
const SPEC_DOSE_MULT = SPEC_UPDATE_HZ / DOSE_REF_FPS;
// Trail/Glow orthogonality (same fix, same reasoning, as Vectorscope/TimeScope/Meter/SpectrumScope
// — see any of theirs for the fuller derivation): at steady state, accumulated brightness scales
// with `glowBase * tau`, so lengthening the trail silently brightens a dwelling signal even with
// glowBase untouched. Folding `TAU_REF/tau` into the dose cancels that, anchored at a real 100 ms
// for the same reason the scope views are: it reads as "glowBase units per 100 ms of persistence",
// physically meaningful rather than tied to any one view's own default trailTau.
//
// This backdrop was deliberately left OUT of that fix when the others got it (see git history):
// it ran phosphor.ts's `over` blend with a `punch` dial (0 = converges on its own colour, never
// blooms; 1 = mathematically identical to `add`, see phosphor.ts's own doc) rather than plain
// `add`, and the orthogonality formula only holds for `add` — `over`'s self-limiting discount stays
// dose-dependent even at punch=0, so it doesn't cancel the same way. Once the *meter* proved a
// plain `add` accumulator could be tuned to resist blowout on real program material without any
// self-limiting compromise, the same approach became worth trying here — `punch` is gone entirely
// now, this always runs `add` (see the backdrop effect below), and it's on the same footing as
// every other view. Confirmed punch=1 (already mathematically ≡ add) as a live baseline first, so
// this switch changes nothing about what's on screen except now being reachable through Trail/Glow
// like everywhere else.
const TAU_REF = 0.1;

/** Live-tunable trail/glow — a gear-icon panel (like the scope views' `.vs-tuning`) rather than
 *  fixed constants, specifically so `tau` can be re-tuned without a recompile: it's re-tuned often
 *  because it has no principled "correct" value — changes to how *smoothed* the incoming data
 *  already is (e.g. cageq-monitor's SPEC_TAU_SECS) directly change how this reads even though
 *  nothing here moved. `glowBase` (overall backdrop brightness) rides along for the same reason a
 *  trail-length re-tune usually wants a brightness re-tune alongside it. `tau` is a genuine
 *  time-constant (seconds), same meaning as every other trailTau in the app — see the backdrop
 *  effect's doc comment for why this used to be `fade`, a flat inverted-direction per-event alpha
 *  with no time-base, and no longer is. */
type SpecParams = { tau: number; tail: number; glowBase: number };
// `old_glowBase * tau/TAU_REF` (0.09 * 0.3/0.1 = 0.27) reproduced the exact pre-switch on-screen
// brightness under the new TAU_REF/tau-corrected formula — confirming the blend swap alone was a
// visual no-op — then hand-retuned live from that baseline to today's value, same as every other
// view's glow default gets touched up after its own anchor change.
const SPEC_DEFAULTS: SpecParams = { tau: 0.4, tail: 12, glowBase: 0.48 };

const GRID_HZ = [20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000];
const F_MIN = 20;
const F_MAX = 20000;
/** The plot rect's top/bottom inset, as a fraction of `height` — SpectrumScope's own literal
 *  `plotTop = H*0.03` value. Exported so App.tsx's meter-bar alignment (`plotBox`, sized to match
 *  this chart's real plot-area Y extent) derives the same number instead of carrying its own
 *  hardcoded copy — that's exactly what went stale the last time `PAD` changed and this wasn't
 *  exported yet.
 *
 *  Was two separate constants (`EQ_TOP_INSET_FRAC`/`EQ_BOTTOM_INSET_FRAC`) while `.eq-chart`'s own
 *  outer box was sized by an uncompensated CSS margin that could overflow `.chart-wrap` — PAD's top
 *  inset briefly had to carry extra fudge on its own to fake a visible gap without touching that
 *  margin, since giving `.eq-chart` a real margin kept breaking live. Now that `.chart-wrap`'s own
 *  height is JS-owned (App.css/App.tsx) and every view shares one shared top inset there instead,
 *  that reason is gone and top/bottom are safely merged back to one number. */
export const EQ_V_INSET_FRAC = 0.03;
// Reference curves (measured raw / target) only count toward the Y auto-scale up to this
// frequency: they diverge sharply in the top octaves (measurement noise + treble roll-off),
// which would otherwise blow the scale out to ±24 dB. Still drawn full-range (then clipped).
const RANGE_F_MAX = 12000;
const fmtHz = (f: number) => (f >= 1000 ? `${f / 1000}k` : `${f}`);

export function EqChart({
  series,
  markers = [],
  refs = [],
  phase,
  nodes,
  spectrumRef,
  eqBands,
  preampDb = 0,
  legendHost,
  minSpan,
  height = 210,
}: {
  series: Series[];
  markers?: Marker[];
  refs?: RefCurve[];
  phase?: PhaseCurve;
  nodes?: Nodes;
  /** Floor for the symmetric Y range (± dB). Lets the caller keep the scale stable across a slot
   *  switch (e.g. the larger of both slots' ranges in Comparison mode) instead of rescaling. */
  minSpan?: number;
  /** Live loopback-FFT spectrum drawn as a backdrop on the log-Hz axis, own scale. A ref (not a
   *  value prop): the caller updates it at up to ~60 fps without triggering a React re-render —
   *  the canvas effect below owns its own rAF loop and reads the ref directly, the same pattern
   *  Vectorscope uses for its sample stream. */
  spectrumRef?: RefObject<SpectrumData | null>;
  /** The applied filter cascade (AutoEq fit + custom). Its magnitude response is removed from
   *  the post-EQ capture per bin so the backdrop shows the **pre-filter** source spectrum. */
  eqBands?: Band[];
  /** The §4.1 loudness-match preamp (dB) applied alongside `eqBands` — also removed from the
   *  backdrop, same as Vectorscope/TimeScope's undistort, so it reconstructs the actual *input*
   *  signal rather than just undoing the filter shape and leaving the preamp's level offset in.
   *  Applies even when `eqBands` is empty (Dry still carries this gain). */
  preampDb?: number;
  /** If given, the legend renders (via portal) into this element instead of inline — used to
   *  place it full-width below the chart+meters row so long labels have room. */
  legendHost?: HTMLElement | null;
  height?: number;
}) {
  const { t } = useTranslation();
  const W = 720;
  const H = height;
  const svgRef = useRef<SVGSVGElement>(null);
  const specCanvasRef = useRef<HTMLCanvasElement>(null); // phosphor spectrum backdrop (imperative)
  // The top-edge stroke's own layer — cleared and fully redrawn every frame instead of running
  // through the phosphor accumulator (see the render effect's own comment for why).
  const specStrokeRef = useRef<HTMLCanvasElement>(null);
  const { params: specParams, setParams: setSpecParams, saveAsDefault: saveSpecDefault, resetToFactory: resetSpecFactory } = useTunableParams(
    "cageq-eqchart-spec-params",
    SPEC_DEFAULTS,
  );
  const [specTuning, setSpecTuning] = useState(false);
  const clipId = useId(); // clips the plotted curves to the plot rect (see refPaths)
  // Manual double-click detection from bubbled `click` events. The native `dblclick` is
  // unreliable here: pointer-capture/preventDefault on a node disrupts its synthesis, and on
  // empty space the two clicks often land on *different* thin children (a gridline vs a
  // curve), so the browser fires no dblclick at all. Bubbled clicks always reach the SVG.
  // Records the previous click's time, *screen* position (so the tolerance is a real pixel
  // distance regardless of how wide the 720-unit viewBox is drawn), and what it was over
  // (node index or null) — the second click must match to count as that gesture's double.
  const lastTap = useRef<{ t: number; x: number; y: number; overIdx: number | null } | null>(null);
  const DBL_MS = 450;
  const DBL_DIST = 16; // screen px — did the pointer barely move between the two clicks?
  const [dragIdx, setDragIdx] = useState<number | null>(null);
  const [hoverIdx, setHoverIdx] = useState<number | null>(null);
  // Legend visibility: `overrides` holds only ids the user has clicked; everything else
  // falls back to its `defaultHidden` prop (so opt-in "nerd" layers start off, and a fresh
  // series obeys its default). Absent from both = visible.
  const [overrides, setOverrides] = useState<Record<string, boolean>>({});
  const defaultHidden = useMemo(() => {
    const m: Record<string, boolean> = {};
    for (const s of series) if (s.defaultHidden) m[s.id] = true;
    for (const r of refs) if (r.defaultHidden) m[r.id] = true;
    for (const mk of markers) if (mk.defaultHidden) m[mk.id] = true;
    if (phase?.defaultHidden) m[phase.id] = true;
    return m;
  }, [series, refs, markers, phase]);
  const isHidden = (id: string) => (id in overrides ? overrides[id] : defaultHidden[id] === true);
  const toggle = (id: string) => setOverrides((o) => ({ ...o, [id]: !isHidden(id) }));

  const phaseOn = !!phase && !isHidden(phase.id);
  // Four margins, all a thin buffer only — a fixed 10px on the log-frequency axis (no natural
  // "percent of W" scale to tie it to) and EQ_V_INSET_FRAC*H on the dB axis, not picked-to-fit
  // values: every axis label (dB, Hz, phase-degree) draws ON the tube itself now, same as the scope
  // views' own on-tube readouts (see the gridlines' own doc), so none of the four needs the larger
  // external margin it used to reserve for that text — including `r`, which no longer varies with
  // `phaseOn` for the same reason.
  const PAD = { l: 10, r: 10, t: H * EQ_V_INSET_FRAC, b: H * EQ_V_INSET_FRAC };

  const { freqs, curves, yMin, yMax, step } = useMemo(() => {
    const freqs = logGrid(480, F_MIN, F_MAX);
    const curves = series.map((s) => composedCurveDb(s.bands, freqs));
    let lo = 0;
    let hi = 0;
    // Auto-range over visible curves and visible marker gains only (hiding a spiky layer
    // lets the rest breathe). The user's own EQ (series) and the fit (markers) use the full
    // range — a deliberate HF boost should be visible. Reference curves (measured raw /
    // target), though, diverge noisily in the top octaves, so those only count up to
    // RANGE_F_MAX; the rest of each ref is still drawn, just clipped to the frame. A series
    // marked `excludeFromScale` is skipped here entirely, same idea for a different reason (see
    // that field's doc) — still drawn, just not allowed to set the axis.
    series.forEach((s, i) => {
      if (isHidden(s.id) || s.excludeFromScale) return;
      for (const v of curves[i]) {
        if (v < lo) lo = v;
        if (v > hi) hi = v;
      }
    });
    for (const m of markers) {
      if (isHidden(m.id)) continue;
      for (const b of m.bands) {
        if (b.gain_db < lo) lo = b.gain_db;
        if (b.gain_db > hi) hi = b.gain_db;
      }
    }
    for (const rc of refs) {
      if (isHidden(rc.id)) continue;
      for (const p of rc.points) {
        if (p.f > RANGE_F_MAX) continue;
        if (p.db < lo) lo = p.db;
        if (p.db > hi) hi = p.db;
      }
    }
    // Symmetric range with a sane floor so a flat curve isn't wildly zoomed. `minSpan` (if given)
    // holds the scale steady across a slot switch — never smaller than what the caller asked for.
    const span = Math.max(6, minSpan ?? 0, Math.ceil(Math.max(Math.abs(lo), Math.abs(hi)) + 1));
    const step = span <= 9 ? 3 : span <= 18 ? 6 : 12;
    return { freqs, curves, yMin: -span, yMax: span, step };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [series, markers, refs, overrides, defaultHidden, minSpan]);

  // Phase curve on the secondary axis (computed on the same freq grid). Its degrees range
  // is symmetric and snapped to 45°, independent of the dB axis.
  const { phaseCurve, phaseRange } = useMemo(() => {
    if (!phase) return { phaseCurve: null as Float64Array | null, phaseRange: 90 };
    const p = phaseDeg(phase.bands, freqs);
    let m = 45;
    for (const v of p) m = Math.max(m, Math.abs(v));
    return { phaseCurve: p, phaseRange: Math.ceil(m / 45) * 45 };
  }, [phase, freqs]);

  const lnMin = Math.log(F_MIN);
  const lnSpan = Math.log(F_MAX) - lnMin;
  const x = (f: number) => PAD.l + ((Math.log(f) - lnMin) / lnSpan) * (W - PAD.l - PAD.r);
  const y = (db: number) => PAD.t + ((yMax - db) / (yMax - yMin)) * (H - PAD.t - PAD.b);
  const yPhase = (deg: number) => PAD.t + ((phaseRange - deg) / (2 * phaseRange)) * (H - PAD.t - PAD.b);
  // Inverse scales, for turning a pointer position back into (frequency, gain).
  const invX = (vx: number) => Math.exp(lnMin + ((vx - PAD.l) / (W - PAD.l - PAD.r)) * lnSpan);
  const invY = (vy: number) => yMax - ((vy - PAD.t) / (H - PAD.t - PAD.b)) * (yMax - yMin);

  /** Pointer client coords -> viewBox coords (the SVG scales to its container). */
  const toViewBox = (clientX: number, clientY: number) => {
    const r = svgRef.current!.getBoundingClientRect();
    return { vx: ((clientX - r.left) / r.width) * W, vy: ((clientY - r.top) / r.height) * H };
  };

  // Q on the wheel. Registered natively with { passive: false } because React's
  // synthetic wheel handler can't preventDefault — without it the page scrolls too.
  useEffect(() => {
    const el = svgRef.current;
    if (!el || !nodes || nodes.disabled) return;
    const onWheel = (e: WheelEvent) => {
      if (hoverIdx == null) return;
      e.preventDefault();
      const band = nodes.bands[hoverIdx];
      if (!band) return;
      const factor = e.deltaY > 0 ? 1 / 1.12 : 1.12;
      nodes.onChange(hoverIdx, { q: Math.round(clamp(band.q * factor, 0.1, 20) * 100) / 100 });
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, [nodes, hoverIdx]);

  // Report the hovered (or dragged) node up so the grid can highlight the matching band —
  // the reverse direction arrives via nodes.hoverIdx. Driven off internal state (not the
  // enter/leave handlers directly) so the leave→enter ordering between adjacent nodes can't
  // leave a stale value; the drag index wins so the link holds even if a fast drag outruns
  // the pointer leaving the circle.
  useEffect(() => {
    nodes?.onHover?.(dragIdx ?? hoverIdx);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hoverIdx, dragIdx]);

  const paths = useMemo(
    () =>
      curves.map((curve) => {
        let d = "";
        for (let i = 0; i < freqs.length; i++) {
          d += `${i ? "L" : "M"}${x(freqs[i]).toFixed(2)},${y(curve[i]).toFixed(2)}`;
        }
        return d;
      }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [freqs, curves, yMin, yMax, H],
  );

  // Reference curves come as raw (f, db) points. Use *real* y values (not clamped to the
  // range) and let the SVG clip mask hide the out-of-plot part — clamping would draw a
  // bogus flat line pinned to the top/bottom edge instead of the line exiting the frame.
  const refPaths = refs.map((rc) => {
    let d = "";
    rc.points.forEach((p, i) => {
      const px = x(clamp(p.f, F_MIN, F_MAX));
      const py = y(p.db);
      d += `${i ? "L" : "M"}${px.toFixed(2)},${py.toFixed(2)}`;
    });
    return d;
  });

  const phasePath = useMemo(() => {
    if (!phaseCurve) return "";
    let d = "";
    for (let i = 0; i < freqs.length; i++) d += `${i ? "L" : "M"}${x(freqs[i]).toFixed(2)},${yPhase(phaseCurve[i]).toFixed(2)}`;
    return d;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [phaseCurve, freqs, phaseRange, PAD.r, H]);

  const dbTicks: number[] = [];
  for (let v = -Math.floor(yMax / step) * step; v <= yMax; v += step) dbTicks.push(v);

  // Legend rows: every series, then reference curves, phase, then markers, in draw order.
  type LegendStyle = "solid" | "dashed" | "dotted" | "diamond";
  const legend: { id: string; color: string; label: string; style: LegendStyle }[] = [
    ...series.map((s) => ({ id: s.id, color: s.color, label: s.label, style: (s.muted ? "dashed" : "solid") as LegendStyle })),
    ...refs.map((rc) => ({ id: rc.id, color: rc.color, label: rc.label, style: "dotted" as LegendStyle })),
    ...(phase ? [{ id: phase.id, color: phase.color, label: phase.label, style: "dashed" as LegendStyle }] : []),
    ...markers.map((m) => ({ id: m.id, color: m.color, label: m.label, style: "diamond" as LegendStyle })),
  ];

  // Phosphor spectrum backdrop: fade the whole canvas a touch, then paint the current spectrum as
  // one column per FFT bin with a bright-at-the-floor gradient. Fade + repaint = persistence — a
  // moving resonance leaves a glowing decaying trail and sustained energy stays lit — the same
  // idea as the level meter, one bar per bin. Imperative, so the SVG doesn't re-render at the
  // spectrum rate. Kept dim (neutral theme colour with a hint of accent tinted in), so it reads
  // as a backdrop and the coloured EQ curves stay legible on top. X = log-Hz; Y = dBFS.
  //
  // Pre-filter view: the capture is the post-EQ output, so subtract the applied filter response
  // (dB) *and* the preamp at each bin's frequency to recover the actual source spectrum — the
  // resonances the EQ is fighting show where they actually are, not flattened by the correction,
  // and Dry's loudness-match preamp doesn't leave a residual level offset. (VU/LUFS keep the raw
  // post-EQ signal; only this display is un-EQ'd.)
  // Render-time values the rAF loop below needs but can't close over directly — the loop is set up
  // once at mount (like Vectorscope's), so anything from props/render has to come through a ref
  // that's refreshed every render, read fresh each frame.
  const specDrawCtx = useRef({ eqBands, preampDb, PAD, H, W, specParams });
  specDrawCtx.current = { eqBands, preampDb, PAD, H, W, specParams };
  // The spectrum backdrop. Each received payload is drawn as a smooth curve (traceSmooth, spline.ts)
  // through the bins' own values — filled down to the baseline for a dim ambient wash, then stroked
  // again on top at full `glowBase` (see the render loop's own comments for why two layers, and why
  // smoothed rather than a per-bin staircase) — into a scratch canvas handed to the shared phosphor
  // accumulator (phosphor.ts), which owns the decay and the composite — the same machinery, same
  // additive blend, the scope views use.
  //
  // This previously kept its own timestamped stamp ring and redrew the whole trail every frame,
  // because an in-place `destination-out` fade can't reach zero: in 8-bit storage it stalls
  // wherever alpha drops below ~0.5/(1-keep) LSB, and a live-tunable `tau` made that trivial to
  // hit — dialing in a long trail meant dialing in a permanent ghost. That turned out to be a
  // property of the *storage*, not of decay-in-place: the WebGL attempt that seemed to prove
  // otherwise had been writing into UNSIGNED_BYTE textures, so its float shader math was rounded
  // straight back into the same trap. Half-float storage removes it, so the ring, its cutoff and
  // capacity constants, and the per-frame history redraw are all gone.
  //
  useEffect(() => {
    const cv = specCanvasRef.current;
    const strokeCv = specStrokeRef.current;
    const strokeCtx = strokeCv?.getContext("2d");
    if (!cv || !strokeCtx) return;
    // Plain additive, same as every scope view — see TAU_REF's own doc above for why this used to
    // be phosphor.ts's `over` blend instead (a backdrop-specific self-limiting compromise) and why
    // that's no longer needed.
    const phos = createPhosphor(cv);
    if (!phos) return;
    let raf = 0;
    let last = performance.now();
    // Each bin's own (x, top-edge y) — the wash fill draws `yScratch` straight; the stroke draws a
    // separately time-smoothed copy of it (`yStrokeSmooth`, below) instead. Computed once per frame
    // so neither redundantly repeats the correction lookup, matching the scope views' own
    // scratch-buffer pattern (e.g. SpectrumScope's vScratch). Fed straight to `traceSmooth`
    // (spline.ts) — a plain per-bin polyline visibly showed the individual FFT bins as a jagged
    // staircase once the stroke below existed to trace it crisply (reported live), most noticeably
    // in the dense top octaves; this is
    // the exact same fix SpectrumScope's own trace already uses on the identical underlying data.
    let xScratch = new Float64Array(0);
    let yScratch = new Float64Array(0);
    // A second, time-smoothed copy of yScratch's values, used only by the stroke below — see its
    // own comment for why. `NaN` marks "not yet initialized" so the first real frame snaps straight
    // to target instead of animating in from zero.
    let yStrokeSmooth = new Float64Array(0).fill(NaN);

    const render = () => {
      const now = performance.now();
      const dt = Math.min(0.1, (now - last) / 1000); // clamp after a tab-switch stall
      last = now;
      const { eqBands, preampDb, PAD, H, W, specParams } = specDrawCtx.current;
      const spectrum = spectrumRef?.current ?? null;
      const ctx = phos.begin();
      // Own layer, cleared and fully redrawn every frame — see the stroke's own comment below for
      // why it doesn't run through the phosphor accumulator like the wash does. Unconditional (not
      // only when `spectrum` is present) so a truly empty frame doesn't leave a stale line sitting
      // on screen forever, the way a non-accumulating layer with nothing decaying it otherwise would.
      strokeCtx.clearRect(0, 0, W, H);

      // Redrawn every animation frame now, not only when a new payload lands: commit()'s own
      // dt-scaled dose (phosphor.ts's DOSE_REF_DT) already keeps the per-second brightness
      // independent of how often this runs, so spreading each update's dose over every frame
      // instead of lumping it into one draw per ~60 Hz spectrum event is dose-neutral — it just
      // stops the lump-then-decay cycle a higher-refresh display was showing as visible flicker
      // (reported live, back when this ran the `over` blend with `punch` above 0 — signal jitter
      // was ruled out since it reproduced against a synthetic pure tone too). Same "gentle
      // fade-away" during silence as before: the backend keeps emitting the same idle-decayed
      // spectrum values: we just redraw whichever one is latest every frame now, instead of only on
      // the frame where the object reference first changed.
      if (spectrum && spectrum.db.length >= 2) {
        const n = spectrum.db.length;
        const lnF0 = Math.log(spectrum.f_min);
        const lnF1 = Math.log(spectrum.f_max);
        const binF = (i: number) => Math.exp(lnF0 + (i / (n - 1)) * (lnF1 - lnF0));
        const fx = (i: number) => PAD.l + ((Math.log(binF(i)) - lnMin) / lnSpan) * (W - PAD.l - PAD.r);
        const plotTop = PAD.t;
        const plotBot = H - PAD.b;
        const sy = (db: number) => plotBot - clamp((db - (SPEC_TOP_DB - SPEC_DYN)) / SPEC_DYN, 0, 1) * (plotBot - plotTop);

        // The EQ response at each bin frequency plus the preamp — both subtracted from the
        // (post-EQ) capture to undo everything applied and recover the actual source. `eqBands` is
        // `undefined` only mid self-test (correction fully off, capture as-is since the EQ is
        // changing under it); empty (Dry — no filters, still the loudness-match preamp) still gets
        // the preamp term.
        const undoing = eqBands !== undefined;
        let corr: Float64Array | null = null;
        if (undoing && eqBands.length) {
          const bf = new Float64Array(n);
          for (let i = 0; i < n; i++) bf[i] = binF(i);
          corr = composedCurveDb(eqBands, bf);
        }

        const [nr, ng, nb] = SPEC_NEUTRAL;
        const acc = parseHex(getComputedStyle(cv).getPropertyValue("--accent"));
        // A hint of the accent mixed into the (fixed, theme-independent — see SPEC_NEUTRAL) neutral
        // base — lifts the glow off flat grey without letting it read as another curve. Only for the
        // wash: it's a dim backdrop by design, and gets its real brightness from accumulation over
        // many frames, not from this base colour's own value. The stroke (below) uses `acc` directly,
        // undimmed — see SPEC_WASH_FRAC's own doc for why sharing this tinted colour didn't work.
        const [r, g, b] = acc
          ? [nr + (acc[0] - nr) * SPEC_TINT, ng + (acc[1] - ng) * SPEC_TINT, nb + (acc[2] - nb) * SPEC_TINT].map(Math.round)
          : [nr, ng, nb];
        const [sr, sg, sb] = acc ?? [nr, ng, nb];

        if (xScratch.length < n) {
          xScratch = new Float64Array(n);
          yScratch = new Float64Array(n);
          const grown = new Float64Array(n).fill(NaN);
          grown.set(yStrokeSmooth); // preserve already-settled bins; new ones start at NaN (unset)
          yStrokeSmooth = grown;
        }
        for (let i = 0; i < n; i++) {
          const db = undoing ? spectrum.db[i] - (corr ? corr[i] : 0) - preampDb : spectrum.db[i];
          xScratch[i] = fx(i);
          yScratch[i] = sy(db);
        }

        // Smoothed copy of yScratch for the stroke only — see STROKE_SMOOTH_TAU's own doc. Plain
        // per-bin exponential decay toward the raw target, dt-scaled the same way every decay in
        // this app is (`exp(-dt/tau)`) so it's frame-rate independent. NaN (unset, a fresh bin) snaps
        // straight to target instead of animating in from zero.
        const strokeK = Math.exp(-dt / STROKE_SMOOTH_TAU);
        for (let i = 0; i < n; i++) {
          yStrokeSmooth[i] = Number.isNaN(yStrokeSmooth[i]) ? yScratch[i] : yScratch[i] + (yStrokeSmooth[i] - yScratch[i]) * strokeK;
        }

        // Ambient wash: the smooth curve (traceSmooth, spline.ts) closed down to the baseline, at a
        // flat, non-gradient alpha — see SPEC_WASH_FRAC's own doc for why a position-graded gradient
        // (in either direction) was the wrong tool here regardless of which end was bright.
        // traceSmooth's own leading `moveTo` becomes this path's true start point, so `closePath`
        // below draws straight back to it — no separate "walk up from the baseline" segment needed.
        ctx.fillStyle = `rgba(${r},${g},${b},${specParams.glowBase * SPEC_WASH_FRAC})`;
        ctx.beginPath();
        traceSmooth(ctx, xScratch, yScratch, n);
        ctx.lineTo(xScratch[n - 1], plotBot); // down to baseline at the right edge
        ctx.lineTo(xScratch[0], plotBot); // across baseline to the left edge
        ctx.closePath(); // straight line back up to the curve's own start
        ctx.fill(); // full, undiminished alpha — see SPEC_UPDATE_HZ's own doc for why the per-second
        // correction happens in commit()'s float dose math below, not here on the 8-bit canvas (a low
        // ctx.globalAlpha on a smooth gradient visibly dithers in this WebView2 build).

        // The informative edge: the same smooth curve, stroked at the live-tunable `glowBase` on its
        // own non-accumulating layer (`strokeCtx`) — a stroke is, by construction, drawn at each X's
        // own Y, so it's anchored to that bin's actual value with no gradient needed at all, but it
        // moves with the real signal every ~60 Hz update. Running it through the phosphor accumulator
        // like the wash (tried first) left multiple recent positions smeared together on top of the
        // wash's own trail — reported live as looking wrong once the stroke was actually legible
        // enough to notice. This is meant to read as "where the signal is *right now*", exactly the
        // "constant redraw every frame" case phosphor.ts's own module doc says must live off the
        // accumulator (same reason Vectorscope's resting spot and TimeScope's peak-hold lines do) —
        // the wash is what shows *recent history*, and only it should decay.
        //
        // Draws `yStrokeSmooth`, not the raw `yScratch` the wash uses: with the stroke off the
        // accumulator, it lost a side effect that came along with being on it — recent frames used
        // to blend together there, which incidentally low-pass-filtered small frame-to-frame Y noise
        // for free. Redrawing a fully independent line every frame exposes that noise directly
        // (reported live as jitter). STROKE_SMOOTH_TAU restores just enough of it, but in the
        // geometry the line is built from rather than the pixels — still exactly one crisp line per
        // frame, no accumulation, no smearing of past positions.
        strokeCtx.strokeStyle = `rgba(${sr},${sg},${sb},${specParams.glowBase})`;
        strokeCtx.lineWidth = Math.max(1, H / SPEC_STROKE_REF_H) * 1.5;
        strokeCtx.lineJoin = "round";
        strokeCtx.beginPath();
        traceSmooth(strokeCtx, xScratch, yStrokeSmooth, n);
        strokeCtx.stroke();
      }

      // Two independent corrections multiplied into one doseMult: SPEC_DOSE_MULT (see its own doc)
      // pins the total ink laid down per second to what the wash's alpha was tuned against,
      // independent of the display's own render rate; TAU_REF/tau (see its own doc, above) cancels
      // Trail/Glow's steady-state coupling so a longer trail doesn't also silently brighten. Neither
      // is scaled by this frame's `dt` — commit()'s own dose math already accounts for `dt` once;
      // doing so again here previously reintroduced refresh-rate-dependent brightness.
      phos.commit(dt, specParams.tau, specParams.tail, SPEC_DOSE_MULT * (TAU_REF / specParams.tau));
      raf = requestAnimationFrame(render);
    };
    raf = requestAnimationFrame(render);
    return () => {
      cancelAnimationFrame(raf);
      phos.dispose();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const legendMarkup = (
    <div className="eq-legend">
      {legend.map((row) => {
        const off = isHidden(row.id);
        return (
          <button
            type="button"
            key={`l${row.id}`}
            className={`eq-legend-item${off ? " off" : ""}`}
            aria-pressed={!off}
            onClick={() => toggle(row.id)}
            title={off ? t("chart.show", { label: row.label }) : t("chart.hide", { label: row.label })}
          >
            <svg className="eq-swatch" viewBox="0 0 18 10" width="18" height="10" aria-hidden="true">
              {row.style === "diamond" ? (
                <path d="M9,1.5L13,5L9,8.5L5,5Z" fill={row.color} fillOpacity={0.8} />
              ) : (
                <line
                  x1={1}
                  x2={17}
                  y1={5}
                  y2={5}
                  stroke={row.color}
                  strokeWidth={row.style === "solid" ? 2.4 : row.style === "dotted" ? 1.6 : 1.6}
                  strokeDasharray={row.style === "dotted" ? "1.5 2.5" : row.style === "dashed" ? "4 3" : undefined}
                  strokeLinecap={row.style === "dotted" ? "round" : undefined}
                />
              )}
            </svg>
            <span>{row.label}</span>
          </button>
        );
      })}
    </div>
  );

  return (
    <div className="eq-chart">
    {/* Dark instrument-screen patch, only when there's a backdrop to put on it (matches the tuning
        gear's own `spectrumRef &&` gate below). Fills the *whole* box (plain CSS `inset:0`), not the
        plot rect — same split SpectrumScope's own `.vs-screen` makes: the screen is the container's
        full bounds, and PAD only insets the *content* drawn inside it (gridlines, curves, labels),
        never the screen's own visible edges. Sizing the screen to PAD directly (tried first) meant
        it could never match the scope tubes' own footprint no matter how far PAD shrank, since it was
        answering a different question — "how much margin does the content want" is not "how big is
        the screen". */}
    {spectrumRef && <div className="eq-spectrum-screen" aria-hidden="true" />}
    {/* phosphor spectrum backdrop — same viewBox coords as the SVG (CSS-scaled to match), behind it */}
    <canvas ref={specCanvasRef} className="eq-spectrum-canvas" width={W} height={H} aria-hidden="true" />
    {/* Top-edge stroke, its own non-accumulating layer above the wash — see the render effect's own
        comment for why this can't share the phosphor-accumulated canvas above. */}
    <canvas ref={specStrokeRef} className="eq-spectrum-canvas" width={W} height={H} aria-hidden="true" />
    <svg
      ref={svgRef}
      viewBox={`0 0 ${W} ${H}`}
      // Container-relative (`.eq-chart`'s own JS-owned box, see .chart-wrap's doc in App.css),
      // not the old intrinsic `height:auto` derived from this SVG's own viewBox ratio — the box's
      // own ratio now comes from the exact same 720:215 division `.chart-wrap`'s JS height uses, so
      // this never needs `preserveAspectRatio="none"` to avoid letterboxing: the ratios genuinely
      // agree, not just approximately.
      style={{ width: "100%", height: "100%", userSelect: "none", touchAction: "none" }}
      role="img"
      aria-label={t("chart.aria")}
      onClick={(e) => {
        // Symmetric double-click gestures (detected manually — see lastTap): a second click
        // close to the first (time + screen distance) *and over the same target* → over that
        // node, remove it; over empty space, create a band at the cursor (x → fc, y → gain).
        // The same-target gate stops an empty click then a nearby node click (or vice versa)
        // from being misread as a delete/add.
        if (!nodes || nodes.disabled) return;
        const prev = lastTap.current;
        const isDouble = prev != null && e.timeStamp - prev.t < DBL_MS && Math.hypot(e.clientX - prev.x, e.clientY - prev.y) < DBL_DIST;
        if (isDouble && hoverIdx != null && prev!.overIdx === hoverIdx) {
          lastTap.current = null; // consume, so a third click doesn't immediately re-fire
          nodes.onRemove?.(hoverIdx);
          setHoverIdx(null); // the hovered index is about to shift under us
          return;
        }
        if (isDouble && hoverIdx == null && prev!.overIdx == null && nodes.onAdd) {
          lastTap.current = null;
          const { vx, vy } = toViewBox(e.clientX, e.clientY);
          if (vx >= PAD.l && vx <= W - PAD.r && vy >= PAD.t && vy <= H - PAD.b) {
            nodes.onAdd(Math.round(clamp(invX(vx), F_MIN, F_MAX)), Math.round(clamp(invY(vy), -20, 20) * 10) / 10);
          }
          return;
        }
        // First click, timed out, moved too far, or a cross-gesture pair → (re)start from here.
        lastTap.current = { t: e.timeStamp, x: e.clientX, y: e.clientY, overIdx: hoverIdx };
      }}
    >
      <defs>
        <clipPath id={clipId}>
          <rect x={PAD.l} y={PAD.t} width={W - PAD.l - PAD.r} height={H - PAD.t - PAD.b} />
        </clipPath>
      </defs>
      {/* dB gridlines + labels, both drawn ON the dark `.eq-spectrum-screen` patch now (see its own
          doc) rather than lines-inside-labels-outside — the same "readout printed directly on the
          tube" convention the scope views use (e.g. SpectrumScope's own FREQ_TICKS labels), not a
          chart with an external axis margin. `currentColor` (the theme's --fg) would be almost
          invisible against a guaranteed-dark background in light mode, so both use the accent-tinted-
          on-dark colouring the scope grids already do (e.g. GRID_ALPHA) instead. */}
      {dbTicks.map((db) => (
        <g key={`db${db}`}>
          <line
            x1={PAD.l}
            x2={W - PAD.r}
            y1={y(db)}
            y2={y(db)}
            stroke="var(--accent)"
            strokeOpacity={db === 0 ? 0.4 : 0.16}
          />
          <text x={PAD.l + 5} y={y(db) - 3} textAnchor="start" fontSize="10" fill="var(--accent)" opacity={0.6}>
            {db > 0 ? `+${db}` : db}
          </text>
        </g>
      ))}

      {/* frequency gridlines + labels — same on-tube, accent-coloured treatment as the dB axis above.
          First/last tick (20 Hz/20 kHz) sit exactly on the plot's own left/right edge (GRID_HZ's own
          extremes are F_MIN/F_MAX), so those two anchor start/end instead of centring, the same way
          SpectrumScope's own FREQ_TICKS labels do — a centred label there would overhang past the
          tube's edge into the corner. */}
      {GRID_HZ.map((f) => (
        <g key={`f${f}`}>
          <line x1={x(f)} x2={x(f)} y1={PAD.t} y2={H - PAD.b} stroke="var(--accent)" strokeOpacity={0.16} />
          <text
            x={x(f) + (f === F_MIN ? 3 : f === F_MAX ? -3 : 0)}
            y={H - PAD.b - 5}
            textAnchor={f === F_MIN ? "start" : f === F_MAX ? "end" : "middle"}
            fontSize="10"
            fill="var(--accent)"
            opacity={0.6}
          >
            {fmtHz(f)}
          </text>
        </g>
      ))}

      {/* secondary phase axis (degrees), right side — only when the phase overlay is shown. Moved
          onto the tube like the dB/Hz labels (same reasoning) — still `phase.color` (a fixed,
          caller-supplied hex, not `currentColor`), so no contrast fix was needed there, just the
          position. */}
      {phaseOn && phase && (
        <g>
          {[phaseRange, 0, -phaseRange].map((deg) => (
            <text key={deg} x={W - PAD.r - 4} y={yPhase(deg) + 3.5} textAnchor="end" fontSize="10" fill={phase.color} opacity={0.85}>
              {deg > 0 ? `+${deg}` : deg}°
            </text>
          ))}
        </g>
      )}

      {/* plotted curves/refs/phase/markers, clipped to the plot rect so out-of-range parts
          leave the frame instead of flattening against the edge */}
      <g clipPath={`url(#${clipId})`}>
      {/* curves — muted context lines first so the edited/active layer draws on top */}
      {series.map((s, i) =>
        s.muted && !isHidden(s.id) ? (
          <path
            key={`c${s.id}`}
            d={paths[i]}
            fill="none"
            stroke={s.color}
            strokeWidth={1.25}
            strokeOpacity={0.45}
            strokeDasharray="4 3"
            strokeLinejoin="round"
          />
        ) : null,
      )}
      {series.map((s, i) =>
        !s.muted && !isHidden(s.id) ? (
          <path key={`c${s.id}`} d={paths[i]} fill="none" stroke={s.color} strokeWidth={2} strokeLinejoin="round" />
        ) : null,
      )}

      {/* reference curves (e.g. AutoEq's ideal correction) — thin dotted context */}
      {refs.map((rc, i) =>
        isHidden(rc.id) ? null : (
          <path
            key={`r${rc.id}`}
            d={refPaths[i]}
            fill="none"
            stroke={rc.color}
            strokeWidth={1.4}
            strokeOpacity={0.7}
            strokeDasharray="1 3"
            strokeLinecap="round"
            strokeLinejoin="round"
          />
        ),
      )}

      {/* phase overlay (right axis) — dashed so it doesn't read as a magnitude curve */}
      {phaseOn && phasePath && (
        <path d={phasePath} fill="none" stroke={phase!.color} strokeWidth={1.5} strokeOpacity={0.85} strokeDasharray="6 3" strokeLinejoin="round" />
      )}

      {/* fixed band markers (e.g. the AutoEq fit) as small diamonds */}
      {markers.map((m) =>
        isHidden(m.id)
          ? null
          : m.bands.map((b, i) => {
              const cx = x(clamp(b.freq_hz, F_MIN, F_MAX));
              const cy = y(b.gain_db); // real y; the clip hides an out-of-range diamond
              const r = 3.4;
              return (
                <path
                  key={`m${m.id}-${i}`}
                  d={`M${cx},${cy - r}L${cx + r},${cy}L${cx},${cy + r}L${cx - r},${cy}Z`}
                  fill={m.color}
                  fillOpacity={0.7}
                  stroke="currentColor"
                  strokeOpacity={0.25}
                  style={{ pointerEvents: "none" }}
                />
              );
            }),
      )}
      </g>

      {/* draggable band handles (§5.2): X = fc, Y = gain, wheel = Q. Hidden entirely when
          disabled (e.g. Dry active) — stale handles from the last slot shouldn't linger. */}
      {nodes && !nodes.disabled && nodes.bands.map((b, i) => {
        const cx = x(clamp(b.freq_hz, F_MIN, F_MAX));
        const cy = y(clamp(b.gain_db, yMin, yMax));
        const active = dragIdx === i || hoverIdx === i || nodes.hoverIdx === i;
        const isNew = nodes.highlightIdx === i;
        // Fixed macro bands (Bass/Treble/Air) ride along at runtime even though the type is
        // Band; they can't be removed, so don't promise it in the tooltip.
        const isFixed = (b as { fixed?: boolean }).fixed === true;
        return (
          <g key={`n${i}`}>
            <title>{isFixed ? t("chart.nodeMacro") : t("chart.nodeEdit")}</title>
            {isNew && (
              <circle cx={cx} cy={cy} r={6} fill="none" stroke={nodes.color} strokeWidth={2} className="eq-node-pulse" />
            )}
            <circle
              cx={cx}
              cy={cy}
              r={active ? 7 : 5.5}
              fill={nodes.color}
              fillOpacity={active ? 0.95 : 0.75}
              stroke="currentColor"
              strokeOpacity={0.35}
              style={{ cursor: nodes.disabled ? "default" : dragIdx === i ? "grabbing" : "grab" }}
              onPointerEnter={() => setHoverIdx(i)}
              onPointerLeave={() => setHoverIdx((h) => (h === i ? null : h))}
              onPointerDown={(e) => {
                if (nodes.disabled) return;
                // Without preventDefault the browser's native selection drag races ours.
                e.preventDefault();
                (e.target as Element).setPointerCapture(e.pointerId);
                setDragIdx(i);
              }}
              onPointerMove={(e) => {
                if (dragIdx !== i) return;
                // A missed pointerup would otherwise drag the node on plain hover; if no
                // button is held, end the drag instead.
                if (e.buttons === 0) {
                  try {
                    (e.target as Element).releasePointerCapture(e.pointerId);
                  } catch {
                    /* capture may already be gone */
                  }
                  setDragIdx(null);
                  nodes.onDragEnd?.();
                  return;
                }
                const { vx, vy } = toViewBox(e.clientX, e.clientY);
                nodes.onChange(i, {
                  freq_hz: Math.round(clamp(invX(vx), F_MIN, F_MAX)),
                  gain_db: Math.round(clamp(invY(vy), -20, 20) * 10) / 10,
                });
              }}
              onPointerUp={(e) => {
                if (dragIdx !== i) return;
                try {
                  (e.target as Element).releasePointerCapture(e.pointerId);
                } catch {
                  /* capture may already be gone */
                }
                setDragIdx(null);
                nodes.onDragEnd?.();
              }}
              onPointerCancel={() => {
                if (dragIdx !== i) return;
                setDragIdx(null);
                nodes.onDragEnd?.();
              }}
            />
            {active && (
              <text x={cx} y={cy - 11} textAnchor="middle" fontSize="9.5" fill="currentColor" opacity={0.8}>
                {b.freq_hz >= 1000 ? `${(b.freq_hz / 1000).toFixed(2)}k` : b.freq_hz} Hz · {b.gain_db > 0 ? "+" : ""}
                {b.gain_db.toFixed(1)} dB · Q {b.q.toFixed(2)}
              </text>
            )}
          </g>
        );
      })}

    </svg>

      {/* Spectrum-backdrop tuning — only when there's a backdrop to tune (see SpecParams' doc
          comment for why fade/glow are live-adjustable rather than fixed constants). Same
          `.vs-tools`/`.vs-tuning` chrome, and now the same *positioning*, as the scope views' own
          gear-icon panels: `.eq-spectrum-screen` fills `.eq-chart`'s whole box, so the shared
          `.eq-chart .vs-tools` CSS rule's fixed 6px offset already lands the gear in the screen's own
          corner, the same as `.vs-tools`'s default does against `.vs-screen` — no inline PAD-based
          positioning needed (an earlier version computed it inline, back when the screen was sized to
          PAD instead of filling the box, and had its own corner to chase). */}
      {spectrumRef && (
        <div className="vs-tools">
          <button
            type="button"
            className={`vs-tool${specTuning ? " on" : ""}`}
            title={t("scope.tune")}
            aria-pressed={specTuning}
            onClick={() => setSpecTuning((v) => !v)}
          >
            ⚙
          </button>
        </div>
      )}
      {spectrumRef && specTuning && (
        <div className="vs-tuning">
          <div className="vs-tune-head">
            <span className="vs-tune-title">{t("scope.tune")}</span>
            <button type="button" className="vs-tune-reset" onClick={saveSpecDefault}>
              {t("scope.saveDefault")}
            </button>
            <button type="button" className="vs-tune-reset" onClick={resetSpecFactory}>
              {t("scope.reset")}
            </button>
            <button
              type="button"
              className="vs-tune-close"
              title={t("scope.close")}
              aria-label={t("scope.close")}
              onClick={() => setSpecTuning(false)}
            >
              ×
            </button>
          </div>
          {/* `e.currentTarget.value` is read synchronously into a local *before* the functional
              setSpecParams updater, not inside it — a functional updater isn't guaranteed to run
              synchronously with the event, and by the time it does, React has already nulled out
              the synthetic event's currentTarget, throwing on read. The scope views' own tuning
              panels dodge this via a `set(key, value)` helper that captures the value the same
              way, just less visibly; inlined here since this is only two fields. */}
          <label className="vs-tune-row">
            <span className="vs-tune-label">{t("scope.trail")}</span>
            <input
              type="range"
              min={0.1}
              max={2.0}
              step={0.1}
              value={specParams.tau}
              onChange={(e) => {
                const tau = Number(e.currentTarget.value);
                setSpecParams((p) => ({ ...p, tau }));
              }}
            />
            <b>{specParams.tau.toFixed(2)}</b>
          </label>
          <label className="vs-tune-row">
            <span className="vs-tune-label">{t("scope.tail")}</span>
            <input
              type="range"
              min={1}
              max={64}
              step={1}
              value={specParams.tail}
              onChange={(e) => {
                const tail = Number(e.currentTarget.value);
                setSpecParams((p) => ({ ...p, tail }));
              }}
            />
            <b>{specParams.tail.toFixed(0)}</b>
          </label>
          <label className="vs-tune-row">
            <span className="vs-tune-label">{t("scope.glow")}</span>
            <input
              type="range"
              min={0.02}
              max={1}
              step={0.02}
              value={specParams.glowBase}
              onChange={(e) => {
                const glowBase = Number(e.currentTarget.value);
                setSpecParams((p) => ({ ...p, glowBase }));
              }}
            />
            <b>{specParams.glowBase.toFixed(2)}</b>
          </label>
        </div>
      )}

      {/* legend — below the plot; click a chip to hide/show. Portaled into `legendHost` (a
          full-width host below the chart+meters row) when given, else rendered inline. */}
      {legendPortal(legendMarkup, legendHost)}
    </div>
  );
}

/** Render `markup` into `host` via a portal when a host is provided, else inline. */
function legendPortal(markup: ReactNode, host?: HTMLElement | null) {
  return host ? createPortal(markup, host) : markup;
}
