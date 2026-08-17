import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { listen, emit } from "@tauri-apps/api/event";
import { composedCurveDb } from "./biquad";
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

/** The tangent at point `i` for a non-uniform cubic Hermite spline — Catmull-Rom's usual tangent
 *  (average of the neighbouring secant slopes) generalized to unequal x-spacing by weighting each
 *  secant by the *opposite* segment's width, so a short adjacent segment pulls the tangent toward
 *  its own slope instead of the far one's. Reduces to the textbook `(y[i+1]-y[i-1])/2` exactly when
 *  spacing is uniform — the plain Catmull-Rom formula assumes that uniformity and distorts the
 *  curve without it, which is exactly the case here once `traceSmooth`'s caller has dropped the
 *  duplicate-value bins (see there): the surviving points are deliberately *not* evenly spaced.
 *  Endpoints just use the one available secant. */
function hermiteTangent(xs: Float64Array, ys: Float64Array, n: number, i: number): number {
  if (i <= 0) return (ys[1] - ys[0]) / (xs[1] - xs[0]);
  if (i >= n - 1) return (ys[n - 1] - ys[n - 2]) / (xs[n - 1] - xs[n - 2]);
  const hL = xs[i] - xs[i - 1];
  const hR = xs[i + 1] - xs[i];
  const sL = (ys[i] - ys[i - 1]) / hL;
  const sR = (ys[i + 1] - ys[i]) / hR;
  return (hR * sL + hL * sR) / (hL + hR);
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
 * accumulating trail is what gives the CRT feel. The live line's frame-to-frame
 * *value* is linearly interpolated between the last two received events (see the trail effect),
 * not snapped straight to the latest one, so it flows continuously at 60 fps. No separate
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
  const wrapRef = useRef<HTMLDivElement>(null);
  const gridRef = useRef<HTMLCanvasElement>(null);
  const trailRef = useRef<HTMLCanvasElement>(null);
  // The last two received spectrum events, plus when the current one landed and how long the gap
  // before it was — the trail effect below linearly interpolates between them over that gap
  // instead of snapping straight to `cur` the instant it arrives, so the line flows at 60 fps
  // despite spectrum events landing well under that rate (see the component doc comment).
  const prevRef = useRef<SpectrumData | null>(null);
  const curRef = useRef<SpectrumData | null>(null);
  const curAtRef = useRef(0);
  const intervalRef = useRef(1000 / 30); // running estimate (ms); a reasonable seed before the 2nd event
  const eqRef = useRef<ScopeEq>({ filters: [], preampDb: 0 });
  const corrCacheRef = useRef<CorrCache | null>(null);
  const [params, setParams] = useState<Params>(DEFAULTS);
  const [tuning, setTuning] = useState(false);
  const paramsRef = useRef(params);
  paramsRef.current = params;

  // Fills the whole chart-wrap (not square, unlike the vectorscope; not sharing a row, unlike the
  // time scope) — both dimensions tracked from `.chart-wrap`'s own `aspect-ratio:720/215`
  // (App.css), so this instrument's height agrees with the Frequency/Time views' viewBox-scaled
  // SVGs instead of drifting from a hardcoded value and shifting the layout on every view switch.
  const [width, setWidth] = useState(320);
  const [height, setHeight] = useState(215);
  useEffect(() => {
    const el = wrapRef.current;
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
      const now = performance.now();
      if (curRef.current) {
        intervalRef.current = now - curAtRef.current;
        prevRef.current = curRef.current;
      }
      curRef.current = s;
      curAtRef.current = now;
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
    if (!cv) return;
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
    // Reused per-point scratch buffers for the spline (see `traceSmooth`) — resized, never
    // reallocated fresh each frame, matching TimeScope's `magScratch` pattern. Sized for the full
    // bin count even though the deduplicated point count is usually smaller.
    let xScratch = new Float64Array(0);
    let yScratch = new Float64Array(0);

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

        // Values are interpolated between the last two spectrum events over the (measured) gap
        // between them rather than snapped to the latest — events land at ~the frame rate but not
        // aligned to it, and snapping made the line visibly step instead of flow.
        const prev = prevRef.current;
        const lerp = prev && prev.db.length === n ? Math.max(0, Math.min(1, (now - curAtRef.current) / intervalRef.current)) : 1;
        if (xScratch.length < n) {
          xScratch = new Float64Array(n);
          yScratch = new Float64Array(n);
        }
        // The dedup: several adjacent log-spaced display bins can land on the same underlying
        // linear FFT bin — always toward the low-frequency end, where the log grid is finer than
        // the FFT's actual (fixed, linear) resolution — and read the identical raw value.
        // Connecting each of those individually, spline or not, draws a flat plateau; skipping the
        // repeats treats the run as the single point it actually represents. Compared on the raw
        // reading, which is what's duplicated (the lerp of two equal values is equally flat).
        // Always keep the last bin so the trace reaches the true right edge.
        let m = 0;
        for (let i = 0; i < n; i++) {
          if (i > 0 && i < n - 1 && s.db[i] === s.db[i - 1]) continue;
          const raw = lerp >= 1 || !prev ? s.db[i] : prev.db[i] + (s.db[i] - prev.db[i]) * lerp;
          const v = corr ? raw - corr[i] : raw;
          const frac = Math.max(0, Math.min(1, (v - (SPEC_TOP_DB - SPEC_DYN)) / SPEC_DYN));
          xScratch[m] = (i / (n - 1)) * W;
          yScratch[m] = plotBot - frac * (plotBot - plotTop);
          m++;
        }
        if (m >= 2) {
          ctx.beginPath();
          traceSmooth(ctx, xScratch, yScratch, m);
          ctx.stroke();
        }
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
    <div className="spectrumscope-wrap" ref={wrapRef}>
      {/* width/height here are CSS "100%" (matching the wrap exactly, sub-pixel precise) — the
          JS-measured `resW`/`resH` state feeds only the canvas backing-store *resolution*
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
          ref={trailRef}
          className="vectorscope-canvas vs-trail"
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
    </div>
  );
}
