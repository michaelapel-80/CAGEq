import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { listen, emit } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { type Band, type BiquadCoeffs, type BiquadState, inverseBiquadCoeffs, zeroState, stepBiquad } from "./biquad";
import type { ScopeData, ScopeEq } from "./Vectorscope";

/** Live-tunable render parameters (see Vectorscope's identical rationale — a live panel beats a
 *  recompile). `undistort` shares Vectorscope's meaning and machinery: it undoes the §4.1/§4.2
 *  preamp *and* inverse-filters the applied EQ, recovering the pre-EQ level and shape rather than
 *  just amplifying the post-EQ trace — a manual gain knob would need re-tuning every time the
 *  preamp changes (a different headphone/target/slot), this tracks it automatically. */
type Params = { trailTau: number; glow: number; beam: number; undistort: boolean };
const DEFAULTS: Params = { trailTau: 0.07, glow: 0.75, beam: 3.0, undistort: true };
const REF_SIZE = 512; // beam width authored against this reference height, then scaled
const GRID_ALPHA = 0.22;
// Peak-hold ballistics for the faint clip-reference lines — instant attack, brief hold, then
// linear-dB release (a PPM-style follower, same shape as the level meter's — values mirror
// cageq-monitor's PEAK_RELEASE_DB_PER_SEC/PEAK_HOLD/DB_FLOOR for a consistent feel app-wide).
// Tracked locally per lane from the *displayed* samples (not the meter's own value) so it stays
// correct in both modes: with undistort on, the meter's raw-output peak wouldn't match a trace
// that's now been amplified/reshaped — the line would sit *inside* the beam it's supposed to
// reference. Tracking the same outL/outR the trace itself draws is correct by construction.
const PEAK_RELEASE_DB_PER_SEC = 20;
const PEAK_HOLD_MS = 350;
const DB_FLOOR = -120;
// Peak *detection* is smoothed through a short one-pole envelope before the block max is taken —
// real peak meters do the same (a brief integration time) so an isolated glitch — lossy-codec
// pre/post-echo artifacts, or ringing from the undistort inverse filter (a deep EQ cut inverts
// into a resonant boost, see inverseBiquadCoeffs in biquad.ts) — reads as a spike smoothed down,
// not a false peak, while a genuine transient (spread over many more samples at any real
// bandwidth) survives close to full height. Time-based, not sample-count, so it's identical at
// 44.1/48/96/192 kHz. Only affects this reference line — the drawn trace itself is untouched.
const PEAK_SMOOTH_MS = 0.3;

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

/** Parse a `#rrggbb` hex (the `--accent` CSS var) to [r,g,b]; a green phosphor fallback. */
function parseHex(hex: string): [number, number, number] {
  const m = hex.trim().match(/^#?([0-9a-f]{6})$/i);
  if (!m) return [80, 220, 140];
  const n = parseInt(m[1], 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

/**
 * §5.3d time-domain scope (v1, free-running — see filter.md discussion): the loopback's L/R
 * channels plotted against time, stacked in two lanes (L on top, R on bottom), each with its own
 * zero-centreline. No triggering yet — each emitted window (§8 `scope` event, ~16 ms of audio) is
 * simply stretched across the full width and redrawn, phosphor-style, on top of the fading
 * previous one. On non-periodic material (most real mixes) consecutive windows won't retrace the
 * same shape, so persistence reads as a soft blur rather than a locked waveform — expected until
 * triggering lands.
 *
 * Deliberately does *not* auto-flag clipping (a v1 threshold highlight was tried and dropped): the
 * §4.2 headroom pre-gain already keeps normal operation away from true digital clipping, streaming
 * loudness normalization mostly erases what "hot" means by the time a listener hears it, and a
 * fixed |sample| threshold can't tell real clipping from a squarish kick transient sitting near
 * full-scale — a false-positive machine, not a useful indicator. Eyeballing flat-topping yourself
 * is still exactly what this view is for; it just shouldn't pretend to automate it.
 *
 * Sits to the left of the vectorscope in the scope chart view — same instrument row, same
 * "loopback monitor" family, sharing its `scope` event stream, its `scope-eq`-driven undistort
 * (inverse-filter cascade), and viewer-count gating with Vectorscope.
 */
export function TimeScope({ height = 215 }: { height?: number }) {
  const { t } = useTranslation();
  const wrapRef = useRef<HTMLDivElement>(null);
  const gridRef = useRef<HTMLCanvasElement>(null);
  const trailRef = useRef<HTMLCanvasElement>(null);
  const scopeRef = useRef<ScopeData | null>(null);
  const [params, setParams] = useState<Params>(DEFAULTS);
  const [tuning, setTuning] = useState(false);
  const paramsRef = useRef(params);
  paramsRef.current = params;
  // L/R (two half-height lanes) vs mixdown (one full-height lane, mono sum) — mixdown trades
  // channel separation for 2x the vertical scale, handy when you just want to see how hot/quiet
  // the signal is rather than compare channels. A quick toggle, not a tuning-panel knob — flipped
  // often enough to want single-click access.
  const [mode, setMode] = useState<"lr" | "mix">("mix");
  const modeRef = useRef(mode);
  modeRef.current = mode;

  // Width tracks the flex row's leftover space (not square, unlike the vectorscope); height is fixed.
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

  // The active EQ cascade (for undistort) + the built inverse cascade and its running state —
  // identical structure to Vectorscope's, independent instance (own running filter state, since
  // this draws its own trace from the same raw stream).
  const eqRef = useRef<ScopeEq>({ filters: [], preampDb: 0 });
  const invRef = useRef<{
    active: boolean;
    filtersRef: Band[] | null;
    rate: number;
    coeffs: BiquadCoeffs[];
    stateL: BiquadState[];
    stateR: BiquadState[];
    gain: number;
  }>({ active: false, filtersRef: null, rate: 0, coeffs: [], stateL: [], stateR: [], gain: 1 });

  // Own the `scope` + `scope-eq` subscriptions — independent of the vectorscope's, so either can be
  // open alone. `set_scope_viewer` is a shared count (see Vectorscope/App), so both registering
  // costs nothing extra beyond the one loopback stream already running while any scope view is open.
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
    void invoke("set_scope_viewer", { active: true });
    return () => {
      active = false;
      unlisteners.forEach((u) => u());
      void invoke("set_scope_viewer", { active: false });
    };
  }, []);

  // Static graticule: a centreline per lane (+ the divider between them, in L/R mode). Redrawn on
  // resize or a mode switch (the lane layout itself changes).
  useEffect(() => {
    const cv = gridRef.current;
    const ctx = cv?.getContext("2d");
    if (!cv || !ctx) return;
    const W = cv.width;
    const H = cv.height;
    ctx.clearRect(0, 0, W, H);
    const [ar, ag, ab] = parseHex(getComputedStyle(cv).getPropertyValue("--accent"));
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
    ctx.fillStyle = `rgba(${ar},${ag},${ab},0.5)`;
    ctx.font = `${Math.round(H * 0.05)}px system-ui, sans-serif`;
    ctx.textBaseline = "middle";
    for (const lane of laneLayout(mode, H)) if (lane.label) ctx.fillText(lane.label, 6, lane.cy - lane.amp * 0.55);
  }, [resW, resH, mode]);

  // The trace: fade-then-draw on rAF, same phosphor persistence as the vectorscope.
  useEffect(() => {
    const cv = trailRef.current;
    const ctx = cv?.getContext("2d");
    if (!cv || !ctx) return;
    const [ar, ag, ab] = parseHex(getComputedStyle(cv).getPropertyValue("--accent"));

    let raf = 0;
    let last = performance.now();
    let drawn: ScopeData | null = null;
    // Per-lane peak-hold (dBFS) and the timestamp its release may resume after — indexed to match
    // `lanes` each frame; resized (and reset) on a mode switch, since "lane 0" means something
    // different in L/R vs mixdown. A brief reset on an intentional mode change is unsurprising.
    let peakDb: number[] = [];
    let peakHoldUntil: number[] = [];

    const render = () => {
      const now = performance.now();
      const dt = Math.min(0.1, (now - last) / 1000);
      last = now;
      const p = paramsRef.current;
      const mode = modeRef.current;
      const W = cv.width;
      const H = cv.height;
      const lanes = laneLayout(mode, H);

      if (peakDb.length !== lanes.length) {
        peakDb = lanes.map(() => DB_FLOOR);
        peakHoldUntil = lanes.map(() => 0);
      }
      // Release: continuous decay once past the hold window, every frame regardless of whether new
      // data arrived this frame (so it doesn't step, it eases down smoothly between scope emits).
      for (let lane = 0; lane < lanes.length; lane++) {
        if (now >= peakHoldUntil[lane]) peakDb[lane] = Math.max(peakDb[lane] - PEAK_RELEASE_DB_PER_SEC * dt, DB_FLOOR);
      }

      const s = scopeRef.current;
      const n = s ? Math.floor(s.xy.length / 2) : 0;
      const isNew = !!s && s.signal && n >= 2 && s !== drawn;
      // Computed up front (before drawing) when new data arrives, so the peak-hold attack below and
      // the trace path further down both read the identical (possibly undistorted) samples.
      let outL: Float64Array | null = null;
      let outR: Float64Array | null = null;
      if (isNew) {
        drawn = s;
        const xy = s!.xy;

        // Undistort: (re)build the inverse cascade when the filters/rate change or the mode turns
        // on, then run each sample back through it — same machinery/rationale as Vectorscope's.
        const eq = eqRef.current;
        const rate = s!.rate && s!.rate > 0 ? s!.rate : 48000;
        const iv = invRef.current;
        if (p.undistort) {
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
        // Runs with an empty cascade too when there's just a preamp to undo (e.g. Dry, which
        // carries the §4.1 loudness-match gain but no EQ) — pure gain recovery, no filtering.
        const undistort = p.undistort && (iv.coeffs.length > 0 || iv.gain !== 1);
        outL = new Float64Array(n);
        outR = new Float64Array(n);
        for (let i = 0; i < n; i++) {
          let l = xy[2 * i];
          let r = xy[2 * i + 1];
          if (undistort) {
            l /= iv.gain; // undo the preamp, then run the inverse cascade sample-by-sample
            r /= iv.gain;
            for (let k = 0; k < iv.coeffs.length; k++) {
              l = stepBiquad(iv.coeffs[k], iv.stateL[k], l);
              r = stepBiquad(iv.coeffs[k], iv.stateR[k], r);
            }
          }
          outL[i] = l;
          outR[i] = r;
        }

        // Attack: this window's peak per lane, from the exact values about to be drawn — correct
        // in both modes by construction (it's the same array the trace path reads below). The
        // rectified signal is run through the short envelope (see PEAK_SMOOTH_MS) before the max,
        // so a single-sample outlier can't set the held peak on its own.
        const smoothAlpha = 1 - Math.exp(-1 / (rate * (PEAK_SMOOTH_MS / 1000)));
        for (let lane = 0; lane < lanes.length; lane++) {
          let env = 0;
          let blockPeak = 0;
          for (let i = 0; i < n; i++) {
            const v = Math.abs(mode === "mix" ? (outL[i] + outR[i]) / 2 : lane === 0 ? outL[i] : outR[i]);
            env += (v - env) * smoothAlpha;
            if (env > blockPeak) blockPeak = env;
          }
          const blockDb = 20 * Math.log10(Math.max(blockPeak, 1e-6));
          if (blockDb >= peakDb[lane]) {
            peakDb[lane] = blockDb;
            peakHoldUntil[lane] = now + PEAK_HOLD_MS;
          }
        }
      }

      // 1) Fade the trace toward transparent.
      const fade = 1 - Math.exp(-dt / p.trailTau);
      ctx.globalCompositeOperation = "destination-out";
      ctx.fillStyle = `rgba(0,0,0,${fade})`;
      ctx.fillRect(0, 0, W, H);

      // 2) Faint peak-hold line(s), mirrored ± around each lane's centreline (a waveform is
      // bipolar; the peak tracked above is a magnitude). Drawn *before* the trace (source-over, not
      // the additive beam blend) so the beam can sit visibly over it wherever they cross, rather
      // than the line painting over the beam.
      ctx.globalCompositeOperation = "source-over";
      ctx.strokeStyle = "rgba(230,162,60,0.35)"; // #e6a23c — same amber as .vbar-peak
      ctx.lineWidth = Math.max(1, H / REF_SIZE);
      ctx.beginPath();
      for (let lane = 0; lane < lanes.length; lane++) {
        const { cy, amp } = lanes[lane];
        const dy = Math.pow(10, peakDb[lane] / 20) * amp;
        ctx.moveTo(0, cy - dy);
        ctx.lineTo(W, cy - dy);
        ctx.moveTo(0, cy + dy);
        ctx.lineTo(W, cy + dy);
      }
      ctx.stroke();

      // 3) The trace itself.
      if (isNew && outL && outR) {
        // L/R: one path per channel/lane. Mixdown: one path, mono sum, the single full-height lane.
        const paths = lanes.map(() => new Path2D());
        const valueAt = (i: number, lane: number): number => (mode === "mix" ? (outL![i] + outR![i]) / 2 : lane === 0 ? outL![i] : outR![i]);
        for (let lane = 0; lane < lanes.length; lane++) {
          const { cy, amp } = lanes[lane];
          const path = paths[lane];
          path.moveTo(0, cy - valueAt(0, lane) * amp);
          for (let i = 1; i < n; i++) path.lineTo((i / (n - 1)) * W, cy - valueAt(i, lane) * amp);
        }
        ctx.globalCompositeOperation = "lighter";
        ctx.lineWidth = Math.max(0.6, p.beam * (H / REF_SIZE));
        ctx.lineJoin = "round";
        ctx.lineCap = "round";
        ctx.strokeStyle = `rgba(${ar},${ag},${ab},${p.glow})`;
        for (const path of paths) ctx.stroke(path);
      }
      raf = requestAnimationFrame(render);
    };
    raf = requestAnimationFrame(render);
    return () => cancelAnimationFrame(raf);
  }, []);

  const set = <K extends keyof Params>(k: K, v: Params[K]) => setParams((prev) => ({ ...prev, [k]: v }));
  type NumKey = "trailTau" | "glow" | "beam";
  const CONTROLS: { key: NumKey; label: string; min: number; max: number; step: number }[] = [
    { key: "trailTau", label: t("scope.trail"), min: 0.02, max: 0.6, step: 0.01 },
    { key: "glow", label: t("scope.glow"), min: 0.05, max: 1, step: 0.05 },
    { key: "beam", label: t("scope.beam"), min: 0.5, max: 8, step: 0.1 },
  ];

  return (
    <div className="timescope-wrap" ref={wrapRef} style={{ height: `${height}px` }}>
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
        <div className="vs-tools">
          <button
            type="button"
            className="vs-tool vs-tool-text"
            title={mode === "lr" ? t("scope.toMixTitle") : t("scope.toLrTitle")}
            onClick={() => setMode((m) => (m === "lr" ? "mix" : "lr"))}
          >
            {mode === "lr" ? t("scope.modeLr") : t("scope.modeMix")}
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
