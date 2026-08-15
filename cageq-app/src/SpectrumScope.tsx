import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { listen } from "@tauri-apps/api/event";
import type { SpectrumData } from "./EqChart";

/** Live-tunable render parameters — same rationale as Vectorscope/TimeScope's panels. `trailTau`
 *  is a genuine canvas-alpha phosphor decay time constant (like the other scope views' Trail),
 *  not a value-domain ease — see `steadyAlpha` below for how it avoids that approach's earlier
 *  flicker/saturation bug. */
type Params = { trailTau: number; glow: number };
const DEFAULTS: Params = { trailTau: 0.05, glow: 0.9 };
const REF_SIZE = 512;
const GRID_ALPHA = 0.22;
// Same fixed dBFS scale as EqChart's spectrum backdrop (§5.4) — consistent reading between the
// Frequency pane's backdrop and this standalone analyzer.
const SPEC_TOP_DB = 0;
const SPEC_DYN = 90;
const F_MIN = 20;
const F_MAX = 20000;
const FREQ_TICKS = [100, 1000, 10000]; // unlabeled-chart-clutter-avoiding minimum: one per decade
const BAR_GAP_FRAC = 0.18; // fraction of each bin's slot left as a gap — distinct bars, not a filled area
const PEAK_RGB = "230,162,60"; // #e6a23c — same amber as the level meter's peak mark and the time scope's
// Mirrors cageq-monitor's SPEC_PEAK_DROP_DB_PER_SEC exactly. The backend already decays peak_db
// itself, but only *samples* of that decay arrive with each spectrum event (~23 Hz, well under the
// 60 fps redraw) — drawing the cap only when a new payload lands left a stepped "ladder" of
// partially-faded lines while dropping, instead of one smooth streak. Re-deriving the same decay
// locally and redrawing every frame (attack still gated to new data) fills in the gaps between
// samples, exactly how TimeScope's own local peak-hold already avoids the same problem.
const SPEC_PEAK_DROP_DB_PER_SEC = 60;
const DB_FLOOR = -120;

/**
 * The alpha to draw at, every frame, so that a region redrawn every single frame at that alpha —
 * faded by `fade` (destination-out) between each redraw — settles at exactly `target` alpha at
 * steady state, not beyond it. A bar chart keeps hitting the same pixels every redraw (unlike a
 * moving trace, which almost never re-covers a pixel), so drawing directly at `target` alpha each
 * time compounds via `source-over`'s blend formula and creeps past it — a stable bar washes out to
 * fully opaque within a few frames, which is what "clipping" the color looked like. Steady state of
 * "fade by f, then re-draw at alpha a, forever" is `a/(1-(1-f)(1-a))`; solving that for the `a` that
 * makes it equal `target` gives the balance point. A *moving* bar still leaves a real decaying
 * trail behind it — the abandoned position simply isn't redrawn, so it just fades on its own.
 */
function steadyAlpha(target: number, fade: number): number {
  const t = Math.min(target, 0.98); // keep the denominator off zero at glow=1, fade→0
  return (t * fade) / (1 - (1 - fade) * t);
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
 * looked like an EQ chart with nothing on it; this is a dedicated bar-graph analyzer sharing the
 * vectorscope/time-scope's visual language (dark `.vs-screen`, cached gradients, the same
 * `.vs-tools`/`.vs-tuning` chrome, and — unlike an earlier draft of this component — the same
 * genuine canvas alpha phosphor persistence, not a value-domain reconstruction of the look).
 * The Frequency pane (`EqChart`, curves + its own spectrum backdrop) is untouched — this only
 * replaces Monitor. A bar chart keeps hitting the same pixels every redraw (unlike a moving trace,
 * which almost never re-covers a pixel), which made a naive fade-and-redraw flicker or saturate
 * depending on the decay rate — see `steadyAlpha` above for the fix.
 *
 * Bars, not a filled curve: each of the backend's log-frequency bins (§5.4 `SpectrumUpdate`) gets
 * its own bar with a small gap from its neighbours, plus a thin peak-hold cap at the bin's
 * backend-decaying `peak_db` (same amber as the level meter's peak mark). Mono — the FFT is
 * computed from the mono-summed loopback, there's no L/R spectrum to split.
 *
 * Owns its own `spectrum` subscription (no `set_scope_viewer`-style gating needed: unlike the
 * heavier `scope` stream, `spectrum` is always emitted whenever monitoring runs — Meter and
 * EqChart already consume it the same passive way).
 */
export function SpectrumScope({ height = 215 }: { height?: number }) {
  const { t } = useTranslation();
  const wrapRef = useRef<HTMLDivElement>(null);
  const gridRef = useRef<HTMLCanvasElement>(null);
  const trailRef = useRef<HTMLCanvasElement>(null);
  const peakRef = useRef<HTMLCanvasElement>(null);
  const dataRef = useRef<SpectrumData | null>(null);
  const [params, setParams] = useState<Params>(DEFAULTS);
  const [tuning, setTuning] = useState(false);
  const paramsRef = useRef(params);
  paramsRef.current = params;

  // Fills the whole chart-wrap (not square, unlike the vectorscope; not sharing a row, unlike the
  // time scope) — width tracks the container, height is fixed.
  const [width, setWidth] = useState(320);
  useEffect(() => {
    const el = wrapRef.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setWidth(Math.max(80, Math.floor(el.getBoundingClientRect().width))));
    ro.observe(el);
    setWidth(Math.max(80, Math.floor(el.getBoundingClientRect().width)));
    return () => ro.disconnect();
  }, []);
  const dpr = Math.min(typeof window !== "undefined" ? window.devicePixelRatio || 1 : 1, 2);
  const resW = Math.round(width * dpr);
  const resH = Math.round(height * dpr);

  // Passive listener — Meter.tsx owns starting/stopping the underlying capture (same pattern as
  // App's `spectrum` subscription for EqChart's backdrop).
  useEffect(() => {
    let active = true;
    let un: (() => void) | undefined;
    void (async () => {
      un = await listen<SpectrumData>("spectrum", (e) => {
        if (active) dataRef.current = e.payload;
      });
    })();
    return () => {
      active = false;
      un?.();
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

  // The bars: genuine phosphor simulation — fade the canvas (destination-out), then redraw the
  // raw current reading every frame at a steady-state-derived alpha (see `steadyAlpha` above).
  useEffect(() => {
    const cv = trailRef.current;
    const ctx = cv?.getContext("2d");
    if (!cv || !ctx) return;
    const [ar, ag, ab] = parseHex(getComputedStyle(cv).getPropertyValue("--accent"));

    let raf = 0;
    let last = performance.now();
    // The bar-fill gradient depends on theme + plot geometry + the current steady-state alpha
    // (which moves with `fade`, i.e. with `dt` — effectively every frame) — cached and rebuilt
    // only when the key actually changes, instead of unconditionally (see EqChart's identical
    // `specGradCache` fix — a fresh CanvasGradient costs the GPU compositor a shader/texture
    // upload each time, a real contributor to the GPU-memory growth diagnosed earlier this session).
    let gradKey = "";
    let grad: CanvasGradient | null = null;

    const render = () => {
      const now = performance.now();
      const dt = Math.min(0.1, (now - last) / 1000);
      last = now;
      const p = paramsRef.current;
      const W = cv.width;
      const H = cv.height;
      const plotTop = H * 0.03;
      const plotBot = H * 0.97;

      const fade = 1 - Math.exp(-dt / p.trailTau);
      ctx.globalCompositeOperation = "destination-out";
      ctx.fillStyle = `rgba(0,0,0,${fade})`;
      ctx.fillRect(0, 0, W, H);
      ctx.globalCompositeOperation = "source-over";

      const s = dataRef.current;
      const n = s?.db.length ?? 0;
      if (n >= 2 && s) {
        const topA = steadyAlpha(p.glow, fade);
        const botA = steadyAlpha(p.glow * 0.35, fade);
        const key = `${plotTop}|${plotBot}|${ar},${ag},${ab}|${topA}|${botA}`;
        if (key !== gradKey) {
          gradKey = key;
          grad = ctx.createLinearGradient(0, plotBot, 0, plotTop);
          grad.addColorStop(0, `rgba(${ar},${ag},${ab},${botA})`);
          grad.addColorStop(1, `rgba(${ar},${ag},${ab},${topA})`);
        }

        // The raw current reading, redrawn every frame regardless of whether it's changed since
        // the last spectrum event — that's what lets the fade above do its job: a bar sitting at a
        // stable value settles at exactly the intended glow (steadyAlpha), and when the reading
        // actually drops, the *abandoned* higher region simply isn't redrawn any more and fades
        // away on its own — the real phosphor trail, not a reconstruction from tracked values.
        ctx.fillStyle = grad!;
        for (let i = 0; i < n; i++) {
          const x0 = (i / n) * W;
          const x1 = ((i + 1) / n) * W;
          const slot = x1 - x0;
          const bx = x0 + (slot * BAR_GAP_FRAC) / 2;
          const bw = Math.max(1, slot * (1 - BAR_GAP_FRAC));
          const frac = Math.max(0, Math.min(1, (s.db[i] - (SPEC_TOP_DB - SPEC_DYN)) / SPEC_DYN));
          const yTop = plotBot - frac * (plotBot - plotTop);
          ctx.fillRect(bx, yTop, bw, plotBot - yTop);
        }
      }
      raf = requestAnimationFrame(render);
    };
    raf = requestAnimationFrame(render);
    return () => cancelAnimationFrame(raf);
  }, []);

  // The peak-hold caps: their own layer, cleared crisply every frame — deliberately *not* run
  // through the bars' phosphor fade. A peak's position is essentially always drifting (continuous
  // 60 dB/s release, see SPEC_PEAK_DROP_DB_PER_SEC), so unlike a bar it rarely truly settles —
  // adding canvas persistence on top of that continuous motion meant every peak was perpetually
  // laying down a fresh trailing streak, and with ~240 of them independently drifting at once, that
  // read as a mess rather than a clean marker. A peak-hold indicator's job is already "where has
  // this recently been" — the same job the bars' own trail now does — so it doesn't need a trail of
  // its own on top of that; it should just read cleanly at the correct height each frame.
  useEffect(() => {
    const cv = peakRef.current;
    const ctx = cv?.getContext("2d");
    if (!cv || !ctx) return;

    let raf = 0;
    let last = performance.now();
    let peakDb: number[] = [];

    const render = () => {
      const now = performance.now();
      const dt = Math.min(0.1, (now - last) / 1000);
      last = now;
      const W = cv.width;
      const H = cv.height;
      const plotTop = H * 0.03;
      const plotBot = H * 0.97;

      ctx.clearRect(0, 0, W, H);

      const s = dataRef.current;
      const n = s?.db.length ?? 0;
      if (n >= 2 && s) {
        if (peakDb.length !== n) peakDb = new Array(n).fill(DB_FLOOR);
        for (let i = 0; i < n; i++) {
          // Release: continuous decay every frame, same rate as the backend's own peak_db, so the
          // cap slides smoothly between spectrum emits instead of only moving when one arrives;
          // attack snaps up to the backend's latest value wherever it's higher.
          peakDb[i] = Math.max(peakDb[i] - SPEC_PEAK_DROP_DB_PER_SEC * dt, DB_FLOOR, i < s.peak_db.length ? s.peak_db[i] : DB_FLOOR);
        }

        ctx.strokeStyle = `rgba(${PEAK_RGB},0.9)`;
        ctx.lineWidth = Math.max(1, H / REF_SIZE) * 1.5;
        ctx.beginPath();
        for (let i = 0; i < n; i++) {
          const x0 = (i / n) * W;
          const x1 = ((i + 1) / n) * W;
          const slot = x1 - x0;
          const bx = x0 + (slot * BAR_GAP_FRAC) / 2;
          const bw = Math.max(1, slot * (1 - BAR_GAP_FRAC));
          const frac = Math.max(0, Math.min(1, (peakDb[i] - (SPEC_TOP_DB - SPEC_DYN)) / SPEC_DYN));
          const y = plotBot - frac * (plotBot - plotTop);
          ctx.moveTo(bx, y);
          ctx.lineTo(bx + bw, y);
        }
        ctx.stroke();
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
  ];

  return (
    <div className="spectrumscope-wrap" ref={wrapRef} style={{ height: `${height}px` }}>
      <div className="vs-screen" style={{ width: "100%", height: `${height}px` }}>
        <canvas
          ref={gridRef}
          className="vectorscope-canvas vs-grid"
          width={resW}
          height={resH}
          style={{ width: `${width}px`, height: `${height}px` }}
          aria-hidden="true"
        />
        <canvas
          ref={trailRef}
          className="vectorscope-canvas vs-trail"
          width={resW}
          height={resH}
          style={{ width: `${width}px`, height: `${height}px` }}
          aria-hidden="true"
        />
        <canvas
          ref={peakRef}
          className="vectorscope-canvas vs-peak"
          width={resW}
          height={resH}
          style={{ width: `${width}px`, height: `${height}px` }}
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
          </div>
        )}
      </div>
    </div>
  );
}
