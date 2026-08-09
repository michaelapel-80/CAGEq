import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { listen, emit } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { type Band, type BiquadCoeffs, inverseBiquadCoeffs } from "./biquad";

/** A stereo vectorscope window from the loopback (see cageq-monitor `ScopeUpdate`): interleaved
 *  `l0, r0, l1, r1, …` sample pairs (≈ -1..1) in capture order, a signal flag, and the mix rate. */
export type ScopeData = { xy: number[]; signal: boolean; rate: number };

/** The active EQ cascade, broadcast by the main window (App) so the scope can inverse-filter the
 *  post-EQ loopback back to the pre-EQ source image (the "undistort" mode). */
type ScopeEq = { filters: Band[]; preampDb: number };
/** Per-biquad running state for the inverse cascade (Direct Form I), one set per channel. */
type BiquadState = { x1: number; x2: number; y1: number; y2: number };
const zeroState = (): BiquadState => ({ x1: 0, x2: 0, y1: 0, y2: 0 });
function stepBiquad(c: BiquadCoeffs, s: BiquadState, x: number): number {
  const y = c.b0 * x + c.b1 * s.x1 + c.b2 * s.x2 - c.a1 * s.y1 - c.a2 * s.y2;
  s.x2 = s.x1;
  s.x1 = x;
  s.y2 = s.y1;
  s.y1 = y;
  return y;
}

// The scope's dark "instrument screen" backdrop is a CSS background on .vs-screen (theme-independent).
// The trace lives on a **transparent** canvas over it and fades with `destination-out` (alpha decay),
// so a faded pixel stalls at ~1/255 alpha ≈ 1 LSB over the backdrop — no additive burn-in and no
// per-pixel clamp (a multiplicative fade toward an *opaque* backdrop can't reach it in 8-bit).

/** Live-tunable render parameters (adjustable in the on-screen panel so tuning isn't a recompile).
 *  `rotate` picks orientation: off = raw X-Y (L→horizontal, R→vertical, mono = 45° diagonal — the
 *  view oscilloscope-music is authored for); on = rotated so mono is vertical, anti-phase horizontal. */
type Params = {
  trailTau: number; // phosphor decay time constant (s) — time-based, so the trail is refresh-independent
  glow: number; // beam brightness at full (slow-beam) intensity; velocity glow dims it from here
  beam: number; // beam line width, in reference px (scaled by the tube size)
  focus: number; // velocity-glow reference: segments shorter than this (ref px) draw full-bright,
  //                longer (faster beam = higher freq) dim as ~1/length — a CRT's constant
  //                energy-per-sample. Larger = weaker effect (more of the trace stays bright).
  radiusFrac: number; // full-scale ring radius as a fraction of the half-size
  gridAlpha: number; // graticule brightness
  rotate: boolean;
  invert: boolean; // undistort: inverse-filter the loopback back to the pre-EQ source image
};
const DEFAULTS: Params = { trailTau: 0.05, glow: 0.60, beam: 1.0, focus: 6, radiusFrac: 0.48, gridAlpha: 0.22, rotate: false, invert: true };
const LABEL_ALPHA = 0.5;
const REF_SIZE = 512; // beam width is authored against this tube size, then scaled
const SQRT2 = Math.SQRT2;
const VEL_BUCKETS = 16; // brightness quantisation for velocity glow (batched strokes, not per-segment)
const VEL_FLOOR = 0.05; // dimmest a fast segment goes (keeps sharp transitions faintly visible)
const VEL_REF_RATE = 48000; // the velocity glow judges beam speed in *time*; the per-sample segment
//   length is scaled to this rate so a given speed reads the same at 44.1/48/96/192 kHz.

/** Parse a `#rrggbb` hex (the `--accent` CSS var) to [r,g,b]; a green phosphor fallback. */
function parseHex(hex: string): [number, number, number] {
  const m = hex.trim().match(/^#?([0-9a-f]{6})$/i);
  if (!m) return [80, 220, 140];
  const n = parseInt(m[1], 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

/**
 * §5.3c stereo vectorscope (X-Y goniometer) — the loopback's left channel against its right, drawn
 * as a **connected beam trace** (consecutive samples joined, like a CRT beam) with phosphor
 * persistence: fade-then-draw on rAF turns motion into a glowing, decaying figure. Continuous lines
 * (not decimated dots) are what render oscilloscope-music Lissajous shapes. Purely a monitor.
 *
 * Two stacked canvases over the dark backdrop: a **static** graticule and a **transparent** trace
 * that fades via `destination-out`. Keeping the graticule off the fading canvas avoids it building
 * up, and the alpha fade avoids the additive-on-opaque burn-in.
 *
 * `fill` sizes the square tube to its container (for the pop-out window); otherwise it's `height`
 * px. `onPopOut`, when given, shows a button to detach the scope into its own larger window.
 */
export function Vectorscope({
  height = 215,
  fill = false,
  onPopOut,
}: {
  height?: number;
  fill?: boolean;
  onPopOut?: () => void;
}) {
  const { t } = useTranslation();
  const wrapRef = useRef<HTMLDivElement>(null);
  const gridRef = useRef<HTMLCanvasElement>(null);
  const trailRef = useRef<HTMLCanvasElement>(null);
  // Latest payload, written by the listener and read by the rAF loop — a ref, not state, so the
  // 60 fps stream drives the imperative canvas without ever re-rendering React.
  const scopeRef = useRef<ScopeData | null>(null);
  const [params, setParams] = useState<Params>(DEFAULTS);
  const [tuning, setTuning] = useState(false);
  const paramsRef = useRef(params);
  paramsRef.current = params;

  // The square tube side, in CSS px: fixed `height` inline, or the container's min side when filling.
  const [side, setSide] = useState(height);
  useEffect(() => {
    if (!fill) {
      setSide(height);
      return;
    }
    const el = wrapRef.current;
    if (!el) return;
    const ro = new ResizeObserver(() => {
      const r = el.getBoundingClientRect();
      setSide(Math.max(80, Math.floor(Math.min(r.width, r.height))));
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, [fill, height]);
  const dpr = Math.min(typeof window !== "undefined" ? window.devicePixelRatio || 1 : 1, 2);
  const res = Math.round(side * dpr); // canvas backing-store resolution

  // The active EQ cascade (for undistort) + the built inverse cascade and its running state. Rebuilt
  // by the rAF loop when the filters/rate change; state persists across frames.
  const eqRef = useRef<ScopeEq>({ filters: [], preampDb: 0 });
  const invRef = useRef<{
    active: boolean;
    filtersRef: Band[] | null; // identity of the cascade the coeffs were built from (rebuild on change)
    rate: number;
    coeffs: BiquadCoeffs[];
    stateL: BiquadState[];
    stateR: BiquadState[];
    gain: number;
  }>({ active: false, filtersRef: null, rate: 0, coeffs: [], stateL: [], stateR: [], gain: 1 });

  // Own the loopback `scope` subscription (samples) + the `scope-eq` broadcast (cascade). Both exist
  // only while this view is mounted (inline or pop-out), so nothing touches React elsewhere. On
  // mount we ask the main window to (re)send the cascade, since events aren't retained.
  useEffect(() => {
    let active = true;
    const unlisteners: (() => void)[] = [];
    void (async () => {
      unlisteners.push(
        await listen<ScopeData>("scope", (e) => {
          if (active) scopeRef.current = e.payload;
        }),
      );
      unlisteners.push(
        await listen<ScopeEq>("scope-eq", (e) => {
          if (active) eqRef.current = e.payload;
        }),
      );
      if (active) void emit("scope-eq-request");
    })();
    return () => {
      active = false;
      unlisteners.forEach((u) => u());
    };
  }, []);

  // Register as a scope viewer so the backend emits the scope stream (gated to when a view is
  // open). The inline view registers itself; the pop-out window's count is managed by the main
  // window (App) via the window's lifecycle, since a closed OS window may not run React cleanup.
  useEffect(() => {
    if (fill) return;
    void invoke("set_scope_viewer", { active: true });
    return () => void invoke("set_scope_viewer", { active: false });
  }, [fill]);

  // Static graticule — drawn on its own canvas, so it never accumulates under the fading trace.
  // Redrawn only when the size or a graticule-affecting parameter changes.
  useEffect(() => {
    const cv = gridRef.current;
    const ctx = cv?.getContext("2d");
    if (!cv || !ctx) return;
    const S = cv.width;
    const c = S / 2;
    const R = S * params.radiusFrac;
    ctx.clearRect(0, 0, S, S);
    const [ar, ag, ab] = parseHex(getComputedStyle(cv).getPropertyValue("--accent"));

    ctx.strokeStyle = `rgba(${ar},${ag},${ab},${params.gridAlpha})`;
    ctx.lineWidth = Math.max(1, S / REF_SIZE);
    ctx.beginPath();
    ctx.arc(c, c, R, 0, Math.PI * 2);
    ctx.moveTo(c, c - R);
    ctx.lineTo(c, c + R);
    ctx.moveTo(c - R, c);
    ctx.lineTo(c + R, c);
    if (!params.rotate) {
      const d = R / SQRT2; // raw X-Y: mono runs the 45° diagonal — draw it as a guide
      ctx.moveTo(c - d, c + d);
      ctx.lineTo(c + d, c - d);
    }
    ctx.stroke();

    ctx.fillStyle = `rgba(${ar},${ag},${ab},${LABEL_ALPHA})`;
    ctx.font = `${Math.round(S * 0.045)}px system-ui, sans-serif`;
    ctx.textAlign = "center";
    ctx.textBaseline = "middle";
    if (params.rotate) {
      ctx.fillText("M", c, c - R * 0.9);
      ctx.fillText("L", c - R * 0.6, c - R * 0.6);
      ctx.fillText("R", c + R * 0.6, c - R * 0.6);
    } else {
      ctx.fillText("L", c - R * 0.88, c - R * 0.1); // Left channel → left end of the horizontal axis
      ctx.fillText("R", c + R * 0.1, c - R * 0.88);
      ctx.fillText("M", c + R * 0.55, c - R * 0.55);
    }
  }, [res, params.rotate, params.radiusFrac, params.gridAlpha]);

  // The trace: a transparent canvas that fades via destination-out and draws the beam additively.
  useEffect(() => {
    const cv = trailRef.current;
    const ctx = cv?.getContext("2d");
    if (!cv || !ctx) return;
    const [ar, ag, ab] = parseHex(getComputedStyle(cv).getPropertyValue("--accent"));

    let raf = 0;
    let last = performance.now();
    let drawn: ScopeData | null = null; // last payload already traced (draw each once)
    let spotX = NaN; // the beam's dwell spot — position + brightness, redrawn every frame so it
    let spotY = NaN; //   holds during silence; updated per window from the beam's mean + path length
    let spotB = 0;

    const render = () => {
      const now = performance.now();
      const dt = Math.min(0.1, (now - last) / 1000); // clamp after a tab-switch stall
      last = now;
      const p = paramsRef.current;

      const S = cv.width;
      const c = S / 2;
      const R = S * p.radiusFrac;
      const scale = p.rotate ? R / SQRT2 : R;
      const r0 = Math.max(1.5, p.beam * (S / REF_SIZE)); // beam/spot base radius

      // 1) Fade the trace toward transparent (alpha decay) — time-based, so the trail length is
      //    identical at any refresh rate. Stalls at ~1/255 alpha, invisible over the backdrop.
      const fade = 1 - Math.exp(-dt / p.trailTau);
      ctx.globalCompositeOperation = "destination-out";
      ctx.fillStyle = `rgba(0,0,0,${fade})`;
      ctx.fillRect(0, 0, S, S);

      // Undistort: (re)build the inverse cascade when the filters/rate change or the mode turns on,
      // then run each sample back through it to recover the pre-EQ source image. State persists
      // across frames (the stream is contiguous at ≤48 kHz), so the inverse IIR stays settled.
      const eq = eqRef.current;
      const rate = scopeRef.current?.rate && scopeRef.current.rate > 0 ? scopeRef.current.rate : 48000;
      const iv = invRef.current;
      if (p.invert) {
        if (!iv.active || iv.filtersRef !== eq.filters || iv.rate !== rate) {
          iv.active = true;
          iv.filtersRef = eq.filters;
          iv.rate = rate;
          iv.coeffs = eq.filters.map((b) => inverseBiquadCoeffs(b, rate)).reverse(); // undo in reverse order
          iv.stateL = iv.coeffs.map(zeroState);
          iv.stateR = iv.coeffs.map(zeroState);
          iv.gain = Math.pow(10, eq.preampDb / 20);
        }
      } else {
        iv.active = false;
      }
      // Undistort still runs with an empty cascade when there's a preamp to undo — e.g. Dry, which
      // has no EQ but carries the §4.1 loudness-match gain; without this its excursion wouldn't
      // match A/B (whose preamp the inverse also removes).
      const undistort = p.invert && (iv.coeffs.length > 0 || iv.gain !== 1);

      // 2) Trace + resting spot. The beam is always somewhere: while it moves we draw the connected
      //    trace (velocity-graded — a fast/high-frequency sweep dims, a slow dwell brightens, the
      //    CRT's constant energy per sample), and every frame we mark the beam's *dwell* with a spot
      //    at the window's mean position. Its brightness rises as the window's path length shrinks
      //    (that same energy concentrated). So silence — samples collapsed to centre, line segments
      //    ~zero-length and invisible — renders as an immediate saturated spot; an empty/absent
      //    window parks it at centre. No silence detection, no dark gap. (Bucketed strokes keep the
      //    velocity glow to VEL_BUCKETS stroke calls, not one per segment.)
      const s = scopeRef.current;
      const idle = !s || !s.signal || s.xy.length < 4;
      if (idle) {
        if (s) drawn = s;
        spotX = c; // resting beam parks at centre, full intensity
        spotY = c;
        spotB = p.glow;
      } else if (s !== drawn) {
        drawn = s;
        const buckets: Path2D[] = [];
        for (let b = 0; b < VEL_BUCKETS; b++) buckets.push(new Path2D());
        // Full-bright segment length (px), scaled to a reference rate: at a higher rate the beam
        // moves less per sample, so shrink the threshold to match → the velocity glow tracks beam
        // *speed* (px/s), not px/sample, and the intensity no longer jumps with the sample rate.
        const kRef = Math.max(0.001, p.focus * (S / REF_SIZE) * (VEL_REF_RATE / rate));
        const xy = s.xy;
        let sumX = 0;
        let sumY = 0;
        let cnt = 0;
        let pathLen = 0;
        // Connect only *within* the window — no bridge from the previous window's last point. Windows
        // aren't guaranteed contiguous (tail-cap drops at high rates, timing jitter), so bridging drew
        // long stray lines that piled into a haze box. The 1-sample gap at each boundary is invisible.
        let prevX = NaN;
        let prevY = NaN;
        for (let i = 0; i + 1 < xy.length; i += 2) {
          let l = xy[i];
          let r = xy[i + 1];
          if (undistort) {
            l /= iv.gain; // undo the preamp, then run the inverse cascade sample-by-sample
            r /= iv.gain;
            for (let k = 0; k < iv.coeffs.length; k++) {
              l = stepBiquad(iv.coeffs[k], iv.stateL[k], l);
              r = stepBiquad(iv.coeffs[k], iv.stateR[k], r);
            }
          }
          const xp = p.rotate ? (r - l) / SQRT2 : l; // rotate −45°: L→up-left, R→up-right, mono→up
          const yp = p.rotate ? (l + r) / SQRT2 : r;
          const px = c + xp * scale;
          const py = c - yp * scale; // canvas y is down
          if (!Number.isNaN(prevX)) {
            const dx = px - prevX;
            const dy = py - prevY;
            const dist = Math.sqrt(dx * dx + dy * dy);
            pathLen += dist;
            const f = dist <= kRef ? 1 : Math.max(VEL_FLOOR, kRef / dist); // ~1/velocity, floored
            let b = (f * VEL_BUCKETS) | 0;
            if (b >= VEL_BUCKETS) b = VEL_BUCKETS - 1;
            buckets[b].moveTo(prevX, prevY);
            buckets[b].lineTo(px, py);
          }
          sumX += px;
          sumY += py;
          cnt++;
          prevX = px;
          prevY = py;
        }
        ctx.globalCompositeOperation = "lighter";
        ctx.lineWidth = Math.max(0.6, p.beam * (S / REF_SIZE));
        ctx.lineJoin = "round";
        // Butt (not round) caps: consecutive samples that land in different velocity buckets are
        // stroked separately, and round caps at their shared point would overlap and add into a
        // bright dot at every such sample. Flat caps meet at the point instead of stacking.
        ctx.lineCap = "butt";
        // Beam blanking: brightness ∝ velocity factor, reaching **zero** at the fastest bucket (a
        // beam moving too fast to expose the phosphor draws nothing). Skipping bucket 0 removes the
        // long straight lines that high-frequency jumps would otherwise leave — the faint web —
        // while the slow/dwell buckets keep their brightness. (b0 is blank, so start at 1.)
        for (let b = 1; b < VEL_BUCKETS; b++) {
          ctx.strokeStyle = `rgba(${ar},${ag},${ab},${(p.glow * b) / (VEL_BUCKETS - 1)})`;
          ctx.stroke(buckets[b]);
        }
        // Dwell spot: the window's whole beam energy concentrated where it barely moved. refL small,
        // so ordinary (moving) material gives ~0 while a still window (DC / silence zeros) saturates.
        const refL = r0 * 2.5;
        spotX = cnt > 0 ? sumX / cnt : c;
        spotY = cnt > 0 ? sumY / cnt : c;
        spotB = (p.glow * refL) / (pathLen + refL);
      }

      // Draw the beam spot every frame (source-over → pinned to full intensity, no charge-up): a
      // white-hot saturated core + accent halo. Skipped once the beam is clearly moving (spotB ≈ 0).
      if (spotB > 0.05 && !Number.isNaN(spotX)) {
        ctx.globalCompositeOperation = "source-over";
        const haloR = r0 * 4;
        const halo = ctx.createRadialGradient(spotX, spotY, 0, spotX, spotY, haloR);
        halo.addColorStop(0, `rgba(${ar},${ag},${ab},${Math.min(1, spotB * 0.2)})`);
        halo.addColorStop(1, `rgba(${ar},${ag},${ab},0)`);
        ctx.fillStyle = halo;
        ctx.beginPath();
        ctx.arc(spotX, spotY, haloR, 0, Math.PI * 2);
        ctx.fill();
        const coreR = r0 * 2.5;
        const core = ctx.createRadialGradient(spotX, spotY, 0, spotX, spotY, coreR);
        core.addColorStop(0, `rgba(255,255,255,${Math.min(1, spotB * 2)})`); // white-hot centre
        core.addColorStop(0.4, `rgba(${ar},${ag},${ab},${Math.min(1, spotB * 1.5)})`);
        core.addColorStop(1, `rgba(${ar},${ag},${ab},0)`);
        ctx.fillStyle = core;
        ctx.beginPath();
        ctx.arc(spotX, spotY, coreR, 0, Math.PI * 2);
        ctx.fill();
      }

      raf = requestAnimationFrame(render);
    };
    raf = requestAnimationFrame(render);
    return () => cancelAnimationFrame(raf);
  }, []);

  const set = <K extends keyof Params>(k: K, v: Params[K]) => setParams((prev) => ({ ...prev, [k]: v }));
  const CONTROLS: { key: keyof Params; label: string; min: number; max: number; step: number }[] = [
    { key: "trailTau", label: t("scope.trail"), min: 0.02, max: 0.6, step: 0.01 },
    { key: "glow", label: t("scope.glow"), min: 0.05, max: 1, step: 0.05 },
    { key: "beam", label: t("scope.beam"), min: 0.5, max: 5, step: 0.1 },
    { key: "focus", label: t("scope.focus"), min: 1, max: 24, step: 0.5 },
    { key: "radiusFrac", label: t("scope.scale"), min: 0.3, max: 0.5, step: 0.01 },
    { key: "gridAlpha", label: t("scope.grid"), min: 0, max: 0.5, step: 0.02 },
  ];

  const canvasStyle = { width: `${side}px`, height: `${side}px` } as const;
  return (
    <div className={`vectorscope-wrap${fill ? " fill" : ""}`} ref={wrapRef} style={fill ? undefined : { height: `${height}px` }}>
      <div className="vs-screen" style={canvasStyle}>
        <canvas ref={gridRef} className="vectorscope-canvas vs-grid" width={res} height={res} style={canvasStyle} aria-hidden="true" />
        <canvas ref={trailRef} className="vectorscope-canvas vs-trail" width={res} height={res} style={canvasStyle} aria-hidden="true" />
        <div className="vs-tools">
          {onPopOut && (
            <button type="button" className="vs-tool" title={t("scope.popOut")} onClick={onPopOut}>
              ⤢
            </button>
          )}
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
            {CONTROLS.map((cc) => {
              const dp = cc.step >= 1 ? 0 : cc.step >= 0.1 ? 1 : 2;
              return (
                <label key={cc.key} className="vs-tune-row">
                  <span className="vs-tune-label">{cc.label}</span>
                  <input
                    type="range"
                    min={cc.min}
                    max={cc.max}
                    step={cc.step}
                    value={params[cc.key] as number}
                    onChange={(e) => set(cc.key, Number(e.currentTarget.value) as Params[typeof cc.key])}
                  />
                  <b>{(params[cc.key] as number).toFixed(dp)}</b>
                </label>
              );
            })}
            <div className="vs-tune-sep" />
            <label className="vs-tune-row vs-tune-check">
              <span className="vs-tune-label">{t("scope.rotate")}</span>
              <input type="checkbox" checked={params.rotate} onChange={(e) => set("rotate", e.currentTarget.checked)} />
            </label>
            <label className="vs-tune-row vs-tune-check" title={t("scope.undistortHint")}>
              <span className="vs-tune-label">{t("scope.undistort")}</span>
              <input type="checkbox" checked={params.invert} onChange={(e) => set("invert", e.currentTarget.checked)} />
            </label>
          </div>
        )}
      </div>
    </div>
  );
}
