import { useEffect, useId, useMemo, useRef, useState, type ReactNode, type RefObject } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import { Band, composedCurveDb, logGrid, phaseDeg } from "./biquad";
import { createPhosphor } from "./phosphor";

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
const SPEC_GLOW_TIP = 0.025; // brightness near the current level (vertical falloff)
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
const SPEC_DEFAULTS: SpecParams = { tau: 0.4, tail: 12, glowBase: 0.25 };

const GRID_HZ = [20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000];
const F_MIN = 20;
const F_MAX = 20000;
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
  // The spectrum glow gradient's stops depend only on theme + plot geometry, never on the spectrum
  // data itself — cached and rebuilt only when those change, instead of every spectrum frame (up to
  // 60/s). A fresh CanvasGradient costs the GPU compositor a shader/texture each time; left
  // unbounded across a long session that's a steady GPU-memory drain invisible in the JS heap (see
  // the identical fix in Vectorscope's dwell-spot bloom).
  const specGradCache = useRef<{ key: string; grad: CanvasGradient } | null>(null);
  const [specParams, setSpecParams] = useState<SpecParams>(SPEC_DEFAULTS);
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

  // The phase overlay needs a right axis (degrees) — reserve room for its labels only when
  // it's actually shown, so the plot doesn't lose width when it isn't.
  const phaseOn = !!phase && !isHidden(phase.id);
  const PAD = { l: 40, r: phaseOn ? 34 : 12, t: 12, b: 24 };

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
  // The spectrum backdrop. Each received payload is drawn as one filled polygon into a scratch
  // canvas and handed to the shared phosphor accumulator (phosphor.ts), which owns the decay and
  // the composite — the same machinery the scope views use, at the `over` blend rather than their
  // additive one.
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
  // One filled polygon per payload (the bins' staircase outline, closed down to the baseline)
  // rather than a `fillRect` per bin: identical shape — adjacent same-height rectangles *are* that
  // polygon — for ~240x fewer draw calls.
  useEffect(() => {
    const cv = specCanvasRef.current;
    if (!cv) return;
    // `over`, not the scopes' additive blend: this is a *backdrop* under the EQ curves, so
    // repeated content must converge on its own colour rather than bloom toward white and compete
    // with them (see phosphor.ts).
    const phos = createPhosphor(cv, "over");
    if (!phos) return;
    let raf = 0;
    let last = performance.now();
    let drawn: SpectrumData | null | undefined; // last payload already drawn; undefined = none yet

    const render = () => {
      const now = performance.now();
      const dt = Math.min(0.1, (now - last) / 1000); // clamp after a tab-switch stall
      last = now;
      const { eqBands, preampDb, PAD, H, W, specParams } = specDrawCtx.current;
      const spectrum = spectrumRef?.current ?? null;
      const ctx = phos.begin();

      // Drawn only when a new payload lands, not every frame — the backdrop's brightness is set by
      // how much arrives per second, so re-adding the same reading on every frame would make it
      // scale with refresh rate. Frames in between just decay, which is also what makes the trail
      // fade away gracefully when monitoring stops rather than vanishing on the next frame.
      const fresh = !!spectrum && spectrum !== drawn && spectrum.db.length >= 2;
      drawn = spectrum;
      if (fresh && spectrum) {
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
        // base — lifts the glow off flat grey without letting it read as another curve.
        const [r, g, b] = acc
          ? [nr + (acc[0] - nr) * SPEC_TINT, ng + (acc[1] - ng) * SPEC_TINT, nb + (acc[2] - nb) * SPEC_TINT].map(Math.round)
          : [nr, ng, nb];
        const gradKey = `${plotBot}|${plotTop}|${r},${g},${b}|${specParams.glowBase}`;
        let grad = specGradCache.current?.key === gradKey ? specGradCache.current.grad : null;
        if (!grad) {
          grad = ctx.createLinearGradient(0, plotBot, 0, plotTop);
          grad.addColorStop(0, `rgba(${r},${g},${b},${specParams.glowBase})`); // brightest at the floor
          grad.addColorStop(1, `rgba(${r},${g},${b},${SPEC_GLOW_TIP})`); // fades out toward the top
          specGradCache.current = { key: gradKey, grad };
        }
        ctx.fillStyle = grad;

        // One filled polygon — the bins' staircase outline closed down to the baseline — rather
        // than a fillRect per bin. Same shape, ~240x fewer draw calls.
        ctx.beginPath();
        ctx.moveTo(fx(0), plotBot); // baseline, left edge — closePath draws the return trip
        for (let i = 0; i < n; i++) {
          const x0 = fx(i);
          const x1 = i < n - 1 ? fx(i + 1) : x0 + 1;
          const db = undoing ? spectrum.db[i] - (corr ? corr[i] : 0) - preampDb : spectrum.db[i];
          const yTop = sy(db);
          ctx.lineTo(x0, yTop); // up (or down) to this bin's top
          ctx.lineTo(x1, yTop); // across its width
          if (i === n - 1) ctx.lineTo(x1, plotBot); // down to baseline at the right edge
        }
        ctx.closePath(); // straight line back along the baseline to the left edge
        ctx.fill();
      }

      phos.commit(dt, specParams.tau, specParams.tail);
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
    {/* phosphor spectrum backdrop — same viewBox coords as the SVG (CSS-scaled to match), behind it */}
    <canvas ref={specCanvasRef} className="eq-spectrum-canvas" width={W} height={H} aria-hidden="true" />
    <svg
      ref={svgRef}
      viewBox={`0 0 ${W} ${H}`}
      style={{ width: "100%", height: "auto", userSelect: "none", touchAction: "none" }}
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
      {/* dB gridlines + labels */}
      {dbTicks.map((db) => (
        <g key={`db${db}`}>
          <line
            x1={PAD.l}
            x2={W - PAD.r}
            y1={y(db)}
            y2={y(db)}
            stroke="currentColor"
            strokeOpacity={db === 0 ? 0.35 : 0.12}
          />
          <text x={PAD.l - 6} y={y(db) + 3.5} textAnchor="end" fontSize="10" fill="currentColor" opacity={0.55}>
            {db > 0 ? `+${db}` : db}
          </text>
        </g>
      ))}

      {/* frequency gridlines + labels */}
      {GRID_HZ.map((f) => (
        <g key={`f${f}`}>
          <line x1={x(f)} x2={x(f)} y1={PAD.t} y2={H - PAD.b} stroke="currentColor" strokeOpacity={0.12} />
          <text x={x(f)} y={H - PAD.b + 13} textAnchor="middle" fontSize="10" fill="currentColor" opacity={0.55}>
            {fmtHz(f)}
          </text>
        </g>
      ))}

      {/* secondary phase axis (degrees), right side — only when the phase overlay is shown */}
      {phaseOn && phase && (
        <g>
          {[phaseRange, 0, -phaseRange].map((deg) => (
            <text key={deg} x={W - PAD.r + 4} y={yPhase(deg) + 3.5} textAnchor="start" fontSize="10" fill={phase.color} opacity={0.85}>
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
          `.vs-tools`/`.vs-tuning` chrome as the scope views' own gear-icon panels. */}
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
            <button type="button" className="vs-tune-reset" onClick={() => setSpecParams(SPEC_DEFAULTS)}>
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
              min={0.02}
              max={0.6}
              step={0.01}
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
              max={0.8}
              step={0.01}
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
