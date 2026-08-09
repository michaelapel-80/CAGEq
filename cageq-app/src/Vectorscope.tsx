import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { listen, emit } from "@tauri-apps/api/event";
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

// The scope is its own always-dark "instrument screen" (independent of the app theme): a real
// oscilloscope glows bright traces on a dark tube, and additive ('lighter') accumulation only reads
// as a glow on a dark background — on a light one it would wash to white. So we fade toward a fixed
// dark backdrop rather than to transparent.
const BG: [number, number, number] = [9, 12, 11];

/** Live-tunable render parameters (adjustable in the on-screen panel so tuning isn't a recompile).
 *  `rotate` picks orientation: off = raw X-Y (L→horizontal, R→vertical, mono = 45° diagonal — the
 *  view oscilloscope-music is authored for); on = rotated so mono is vertical, anti-phase horizontal. */
type Params = {
  trailTau: number; // phosphor decay time constant (s) — time-based, so the trail is refresh-independent
  glow: number; // beam brightness (additive) — overlaps/slow segments build the glow
  beam: number; // beam line width, in reference px (scaled by the tube size)
  radiusFrac: number; // full-scale ring radius as a fraction of the half-size
  gridAlpha: number; // graticule brightness
  rotate: boolean;
  invert: boolean; // undistort: inverse-filter the loopback back to the pre-EQ source image
};
const DEFAULTS: Params = { trailTau: 0.16, glow: 0.5, beam: 1.5, radiusFrac: 0.44, gridAlpha: 0.22, rotate: false, invert: false };
const LABEL_ALPHA = 0.5;
const REF_SIZE = 512; // beam width is authored against this tube size, then scaled
const SQRT2 = Math.SQRT2;

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
  const canvasRef = useRef<HTMLCanvasElement>(null);
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
  // by the rAF loop when the filters/rate change (keyed by `invSig`); state persists across frames.
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

  // Prime the backdrop whenever the backing store is (re)sized — setting canvas.width clears it.
  useEffect(() => {
    const ctx = canvasRef.current?.getContext("2d");
    if (!ctx) return;
    ctx.fillStyle = `rgb(${BG[0]},${BG[1]},${BG[2]})`;
    ctx.fillRect(0, 0, res, res);
  }, [res]);

  useEffect(() => {
    const cv = canvasRef.current;
    const ctx = cv?.getContext("2d");
    if (!cv || !ctx) return;
    const cs = getComputedStyle(cv);
    const [ar, ag, ab] = parseHex(cs.getPropertyValue("--accent"));

    let raf = 0;
    let last = performance.now();
    let drawn: ScopeData | null = null; // last payload already traced (draw each once)
    let lastX = NaN; // final beam point of the previous window, to bridge frames continuously
    let lastY = NaN; //   (reset to NaN on idle so a silence gap doesn't draw a stray bridge)

    const render = () => {
      const now = performance.now();
      const dt = Math.min(0.1, (now - last) / 1000); // clamp after a tab-switch stall
      last = now;
      const p = paramsRef.current;

      const S = cv.width;
      const c = S / 2;
      const R = S * p.radiusFrac; // full-scale ring radius
      // Raw X-Y maps a full-scale channel (±1) to the ring; the rotated view maps mono full-scale
      // (|y'| = √2) to it, so single channels land at ~0.7 R — the audio-standard −3 dB corner.
      const scale = p.rotate ? R / SQRT2 : R;

      // 1) Fade the whole screen toward the backdrop (time-based → refresh-rate independent).
      const fade = 1 - Math.exp(-dt / p.trailTau);
      ctx.globalCompositeOperation = "source-over";
      ctx.fillStyle = `rgba(${BG[0]},${BG[1]},${BG[2]},${fade})`;
      ctx.fillRect(0, 0, S, S);

      // 2) Graticule — redrawn every frame at constant alpha so it stays put while the trace decays.
      ctx.strokeStyle = `rgba(${ar},${ag},${ab},${p.gridAlpha})`;
      ctx.lineWidth = Math.max(1, S / REF_SIZE);
      ctx.beginPath();
      ctx.arc(c, c, R, 0, Math.PI * 2);
      ctx.moveTo(c, c - R);
      ctx.lineTo(c, c + R);
      ctx.moveTo(c - R, c);
      ctx.lineTo(c + R, c);
      if (!p.rotate) {
        const d = R / SQRT2; // raw X-Y: mono runs the 45° diagonal — draw it as a guide
        ctx.moveTo(c - d, c + d);
        ctx.lineTo(c + d, c - d);
      }
      ctx.stroke();

      ctx.fillStyle = `rgba(${ar},${ag},${ab},${LABEL_ALPHA})`;
      ctx.font = `${Math.round(S * 0.045)}px system-ui, sans-serif`;
      ctx.textAlign = "center";
      ctx.textBaseline = "middle";
      if (p.rotate) {
        ctx.fillText("M", c, c - R * 0.9);
        ctx.fillText("L", c - R * 0.6, c - R * 0.6);
        ctx.fillText("R", c + R * 0.6, c - R * 0.6);
      } else {
        ctx.fillText("L", c + R * 0.88, c - R * 0.1);
        ctx.fillText("R", c + R * 0.1, c - R * 0.88);
        ctx.fillText("M", c + R * 0.55, c - R * 0.55);
      }

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
      const undistort = p.invert && iv.coeffs.length > 0;

      // 3) Trace the newest window once — a polyline through consecutive samples (the beam path),
      //    bridged from the previous window's last point so the trace is continuous across frames.
      const s = scopeRef.current;
      if (s && s !== drawn) {
        drawn = s;
        if (s.signal && s.xy.length >= 4) {
          ctx.globalCompositeOperation = "lighter";
          ctx.strokeStyle = `rgba(${ar},${ag},${ab},${p.glow})`;
          ctx.lineWidth = Math.max(0.6, p.beam * (S / REF_SIZE));
          ctx.lineJoin = "round";
          ctx.lineCap = "round";
          const xy = s.xy;
          ctx.beginPath();
          if (!Number.isNaN(lastX)) ctx.moveTo(lastX, lastY);
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
            if (i === 0 && Number.isNaN(lastX)) ctx.moveTo(px, py);
            else ctx.lineTo(px, py);
            lastX = px;
            lastY = py;
          }
          ctx.stroke();
          ctx.globalCompositeOperation = "source-over";
        } else {
          lastX = NaN; // idle window → drop the bridge so silence doesn't streak the screen
          lastY = NaN;
        }
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
    { key: "radiusFrac", label: t("scope.scale"), min: 0.3, max: 0.5, step: 0.01 },
    { key: "gridAlpha", label: t("scope.grid"), min: 0, max: 0.5, step: 0.02 },
  ];

  return (
    <div className={`vectorscope-wrap${fill ? " fill" : ""}`} ref={wrapRef} style={fill ? undefined : { height: `${height}px` }}>
      <div className="vs-screen" style={{ width: `${side}px`, height: `${side}px` }}>
        <canvas
          ref={canvasRef}
          className="vectorscope-canvas"
          width={res}
          height={res}
          style={{ width: `${side}px`, height: `${side}px` }}
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
            {CONTROLS.map((cc) => (
              <label key={cc.key} className="vs-tune-row">
                <span>{cc.label}</span>
                <input
                  type="range"
                  min={cc.min}
                  max={cc.max}
                  step={cc.step}
                  value={params[cc.key] as number}
                  onChange={(e) => set(cc.key, Number(e.currentTarget.value) as Params[typeof cc.key])}
                />
                <b>{(params[cc.key] as number).toFixed(2)}</b>
              </label>
            ))}
            <label className="vs-tune-row vs-tune-check">
              <span>{t("scope.rotate")}</span>
              <input type="checkbox" checked={params.rotate} onChange={(e) => set("rotate", e.currentTarget.checked)} />
            </label>
            <label className="vs-tune-row vs-tune-check" title={t("scope.undistortHint")}>
              <span>{t("scope.undistort")}</span>
              <input type="checkbox" checked={params.invert} onChange={(e) => set("invert", e.currentTarget.checked)} />
            </label>
            <button type="button" className="vs-tune-reset" onClick={() => setParams(DEFAULTS)}>
              {t("scope.reset")}
            </button>
          </div>
        )}
      </div>
    </div>
  );
}
