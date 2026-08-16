import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { listen, emit } from "@tauri-apps/api/event";
import { composedCurveDb } from "./biquad";
import { spectrumStream } from "./streams";
import type { SpectrumData } from "./EqChart";
import type { ScopeEq } from "./Vectorscope";

/** Live-tunable render parameters — same rationale as Vectorscope/TimeScope's panels. `trailTau`
 *  is a genuine phosphor decay time constant (like the other scope views' Trail, though here each
 *  trail layer's brightness is recomputed from its age every frame — see the trail effect), not a
 *  value-domain ease. `undistort` shares the scope family's meaning and machinery (same
 *  `scope-eq` broadcast, same "filters + preamp" correction EqChart's own spectrum backdrop now
 *  applies) — see `getCorrection` below for why it's a frequency-domain subtraction here rather
 *  than the scopes' sample-domain inverse cascade: this view never sees raw samples, only the
 *  backend's already-FFT'd, already-log-binned dB values. */
type Params = { trailTau: number; glow: number; undistort: boolean };
const DEFAULTS: Params = { trailTau: 0.15, glow: 0.4, undistort: true };

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

// --- Windowed-history phosphor persistence (see the trail effect's doc comment) ---------------
// A trail layer older than this many trailTau time constants is not drawn at all — e^-5 ≈ 0.7% of
// its original brightness, safely invisible — which is what makes burn-in *structurally*
// impossible here: expiry is a hard cutoff on what gets painted, not a decay some stored image has
// to actually reach.
const TRAIL_CUTOFF_EFOLDS = 5;
// The history ring's capacity: one stamp per rendered frame, so ~4 s at 60 fps — comfortably past
// the longest TRAIL_CUTOFF_EFOLDS × trailTau window (3 s). Slots are preallocated Float64Arrays
// reused in place, so the steady state allocates nothing per frame. Every live stamp is stroked
// individually, each at its true decayed alpha — an earlier draft strided the window down to a
// fixed stroke budget with an "alpha boost" standing in for the skipped neighbours, which
// silently destroyed the accumulation look: the boost is only valid where neighbours actually
// overlap (a dwelling line), but on *moving* content it inflated a single sweep pass to the same
// brightness as a dwell stack — and that single-pass-vs-many-passes contrast IS the phosphor
// overdraw effect. The cost stays manageable because trail layers are stroked as plain polylines
// (see the draw loop); this cap (≈ MAX_TRAIL_STAMPS strokes worst case) is the knob if it ever
// isn't.
const MAX_TRAIL_STAMPS = 240;

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
 * here is recomputed from a window of recent spectrum events every frame instead; the trail
 * effect's doc comment carries the full case history). The Frequency pane (`EqChart`, curves +
 * its own spectrum backdrop) is untouched — this only replaces Monitor.
 *
 * A connected spline through the backend's log-frequency bins (§5.4 `SpectrumUpdate`), not filled
 * bars — an earlier bar-graph version read as flat/clean rather than CRT-like; the trail of
 * discrete decaying event curves is what gives the CRT feel. The live line's frame-to-frame
 * *value* is linearly interpolated between the last two received events (see the trail effect),
 * not snapped straight to the latest one, so it flows continuously at 60 fps. No separate
 * peak-hold marker — the trail already shows "where this has recently been" on its own; a second
 * indicator doing the same job was redundant. Mono — the FFT is computed from the mono-summed
 * loopback, there's no L/R spectrum to split.
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

  // The main trace + its phosphor trail, redrawn IN FULL from a stamp ring every frame — the
  // canvas is hard-cleared, then each retained stamp is stroked at `glow · exp(-age/trailTau)`,
  // oldest first, newest on top, with anything older than TRAIL_CUTOFF_EFOLDS·trailTau simply not
  // drawn. Persistence is *recomputed from timestamps*, not accumulated in an image, and that is
  // the entire point:
  //
  // Every stored-image decay scheme tried before this — and there were several — broke on this
  // rendering stack in ways pure reasoning kept failing to predict, so, recorded here to stay dead:
  // (1) canvas `destination-out` fading — multiplicative, so it can never reach zero, and in 8-bit
  // round-to-nearest storage it stalls outright: alpha below ~0.5/(1-keep) LSB freezes forever,
  // ~7% alpha at this instrument's slow trails (the other scope views' fast trails put the stall
  // at an invisible ~1-2 LSB, which is why filter.md's old "stalls harmlessly" note held there);
  // (2) an SVG alpha-floor filter via `ctx.filter = url(#id)` — silently never took hold in this
  // WebView2 build; (3) a periodic full clear — bounded the residue but wiped legitimate layers
  // with it, a build-up-then-pop worse than the residue; (4) a WebGL ping-pong accumulator with
  // float-math decay + a linear drain term — mathematically bounded to a ~4 s worst-case tail, yet
  // measured ~30 s on this stack for reasons never diagnosed (along the way its upload path also
  // exposed UNPACK_PREMULTIPLY_ALPHA_WEBGL as another silent no-op here, turning the composite
  // additive and clipping the trail to white). Four substrate betrayals is enough: with the trail
  // recomputed from data each frame there is no stored image to decay, so there is nothing that
  // CAN burn in — expiry is a hard cutoff on what gets painted, not a level some pixel has to
  // manage to reach. (The actual burn-in that started all this had a second, independent cause
  // fixed at the source: the backend's idle-decay sweep was being recorded into the trail after
  // the music stopped — see `SpectrumUpdate.signal` and the stamp-recording gate on it below.)
  //
  // What gets recorded matters as much as how it decays: one stamp of the *interpolated live
  // line per rendered frame* — not one entry per received spectrum event. The old accumulator's
  // dwell saturation (a line sitting still overdraws itself toward solid — the CRT look) came
  // from stacking per-frame stamps: its steady state was exactly Σ glow·e^(-age/τ) over past
  // *frames*. An earlier draft of this design recorded per-event curves instead, which thinned the
  // stack below the saturation threshold and visibly killed the overdraw; stamping per frame makes
  // the windowed sum term-for-term identical to the accumulator's — same saturation, same trail —
  // while remaining recomputed-from-data. The stamps live in a fixed ring of reused Float64Arrays
  // (MAX_TRAIL_STAMPS — see there for why every live stamp is stroked individually rather than
  // strided down to a budget), so the steady state allocates nothing per frame. Side benefit of
  // recomputing: the Trail/Glow sliders act retroactively on the already-visible trail — turning
  // Trail down instantly shortens the visible history.
  useEffect(() => {
    const cv = trailRef.current;
    const ctx = cv?.getContext("2d");
    if (!cv || !ctx) return;
    const [ar, ag, ab] = parseHex(getComputedStyle(cv).getPropertyValue("--accent"));

    let raf = 0;
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
    // The stamp ring: raw interpolated db values (pre-correction, so the undistort toggle also
    // acts retroactively on the trail) + each stamp's timestamp. `head` is the next write slot;
    // `count` the number of live entries, trimmed from the old end as stamps expire.
    const ringBuf: (Float64Array | null)[] = new Array(MAX_TRAIL_STAMPS).fill(null);
    const ringT = new Float64Array(MAX_TRAIL_STAMPS);
    let ringHead = 0;
    let ringCount = 0;
    let ringN = 0; // bin count the ring was recorded at; a mismatch (rate switch) resets it

    const render = () => {
      const now = performance.now();
      const p = paramsRef.current;
      const W = cv.width;
      const H = cv.height;
      const plotTop = H * 0.03;
      const plotBot = H * 0.97;

      ctx.clearRect(0, 0, W, H);
      // Additive, not source-over: source-over is a weighted blend *toward* the stroke color, so
      // repeated strokes converge on fully-opaque accent blue and stop — it structurally cannot
      // exceed its own hue, no matter the alpha or how many layers stack. "lighter" sums R/G/B
      // independently with no hue ceiling, so a genuine dwell keeps adding light until each
      // channel clips at 255 — summed, white — which is the actual mechanism behind the
      // vectorscope/time-scope's overexposed-dwell look (both use "lighter" for exactly this).
      // clearRect above is exempt from compositing by spec, so this is safe to set once up front.
      ctx.globalCompositeOperation = "lighter";

      const s = curRef.current;
      const n = s?.db.length ?? 0;
      const corr = p.undistort && s ? getCorrection(corrCacheRef, eqRef.current, s) : null;

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

      // 1) Record this frame's stamp: the live line's values, interpolated between the last two
      // spectrum events over the (measured) gap between them rather than snapped to the latest —
      // events land at ~the frame rate but not aligned to it, and snapping made the line visibly
      // step instead of flow. Beam off on silence (`signal`, the backend's same test the meter
      // uses): nothing is recorded — the backend's idle-decay sweep to the floor must not enter
      // the trail, or it'd repaint itself as exactly the burn-in haze this design exists to kill —
      // and the existing stamps just age out.
      if (n >= 2 && s && s.signal) {
        if (ringN !== n) {
          ringN = n;
          ringHead = 0;
          ringCount = 0;
        }
        let buf = ringBuf[ringHead];
        if (!buf || buf.length !== n) {
          buf = new Float64Array(n);
          ringBuf[ringHead] = buf;
        }
        const prev = prevRef.current;
        const lerp = prev && prev.db.length === n ? Math.max(0, Math.min(1, (now - curAtRef.current) / intervalRef.current)) : 1;
        for (let i = 0; i < n; i++) {
          buf[i] = lerp >= 1 || !prev ? s.db[i] : prev.db[i] + (s.db[i] - prev.db[i]) * lerp;
        }
        ringT[ringHead] = now;
        ringHead = (ringHead + 1) % MAX_TRAIL_STAMPS;
        if (ringCount < MAX_TRAIL_STAMPS) ringCount++;
      }

      // 2) Expire from the old end, then stroke EVERY live stamp oldest→newest (newest lands on
      // top), each at its true decayed alpha — no striding, no compensation (see MAX_TRAIL_STAMPS
      // for why an earlier stroke-budget scheme flattened the accumulation contrast). Where the
      // line dwells, stamps overlap and source-over stacks them toward saturation; where it swept
      // through once, a pixel got one faint pass — that brightness ratio is the phosphor overdraw.
      // Every layer gets the full spline — a cheaper polyline was tried for the trail layers, but
      // at longer trails the layers ARE most of what's on screen, and their ridge shapes are read
      // (that's this instrument's use), so the corners showed exactly where the spline matters
      // most: the deduped, sparse low end. If the worst case (~MAX_TRAIL_STAMPS spline strokes)
      // ever stutters, thin the *stamp recording* rate rather than the rendering — uniformly fewer
      // layers dims the trail uniformly, preserving the dwell-vs-sweep contrast that striding broke.
      const cutoffMs = p.trailTau * 1000 * TRAIL_CUTOFF_EFOLDS;
      while (ringCount > 0 && now - ringT[(ringHead - ringCount + MAX_TRAIL_STAMPS) % MAX_TRAIL_STAMPS] > cutoffMs) {
        ringCount--;
      }
      for (let k = ringCount - 1; k >= 0; k--) {
        const idx = (ringHead - 1 - k + MAX_TRAIL_STAMPS * 2) % MAX_TRAIL_STAMPS;
        const db = ringBuf[idx]!;
        // No k === 0 special case pinning the newest at alpha 1: a fresh stamp's decay factor is
        // ~1 anyway, and during silence (no new stamps) the newest is *old* — pinning it kept a
        // full-brightness line frozen on screen through the fade, then popped it at the cutoff.
        ctx.globalAlpha = Math.exp(-(now - ringT[idx]) / 1000 / p.trailTau);
        // The dedup inside: several adjacent log-spaced display bins can land on the same
        // underlying linear FFT bin — always toward the low-frequency end, where the log grid is
        // finer than the FFT's actual (fixed, linear) resolution — and read the identical raw
        // value. Connecting each of those individually, spline or not, draws a flat plateau;
        // skipping the repeats treats the run as the single point it actually represents. (The
        // lerp of two equal plateau values is equally plateau'd, so stamping doesn't break the
        // equality test.) Always keep the last bin so the trace reaches the true right edge.
        if (xScratch.length < db.length) {
          xScratch = new Float64Array(db.length);
          yScratch = new Float64Array(db.length);
        }
        let m = 0;
        for (let i = 0; i < db.length; i++) {
          if (i > 0 && i < db.length - 1 && db[i] === db[i - 1]) continue;
          const v = corr && corr.length === db.length ? db[i] - corr[i] : db[i];
          const frac = Math.max(0, Math.min(1, (v - (SPEC_TOP_DB - SPEC_DYN)) / SPEC_DYN));
          xScratch[m] = (i / (db.length - 1)) * W;
          yScratch[m] = plotBot - frac * (plotBot - plotTop);
          m++;
        }
        if (m >= 2) {
          ctx.beginPath();
          traceSmooth(ctx, xScratch, yScratch, m);
          ctx.stroke();
        }
      }
      ctx.globalAlpha = 1;

      raf = requestAnimationFrame(render);
    };
    raf = requestAnimationFrame(render);
    return () => cancelAnimationFrame(raf);
  }, []);

  const set = <K extends keyof Params>(k: K, v: Params[K]) => setParams((prev) => ({ ...prev, [k]: v }));
  type NumKey = "trailTau" | "glow";
  const CONTROLS: { key: NumKey; label: string; min: number; max: number; step: number }[] = [
    { key: "trailTau", label: t("scope.trail"), min: 0.02, max: 0.6, step: 0.01 },
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
