import { useMemo, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import { Band, FS, impulseResponse } from "./biquad";
import { EQ_V_INSET_FRAC } from "./EqChart";

/**
 * §5.2 impulse-response view — the applied filter chain's decay, shown *in place of* the
 * magnitude chart (a view swap, not an extra strip, so it costs no vertical space). Pure
 * client-side biquad maths (biquad.ts).
 *
 * On a **dB envelope** axis, not linear: an EQ's impulse is dominated by the n=0 spike, so
 * a resonance's ring sits tens of dB below it and is invisible on a linear plot. In dB the
 * ring's decay is visible, and the time window auto-sets to where the envelope falls below
 * a floor — so a high-Q boost frames its long ring and a broad shelf its quick settle.
 *
 * Same dark `.eq-chart-screen` instrument look and on-tube, accent-coloured axis labels EqChart's
 * own gridlines use (see that file's doc) — this used to run `currentColor` labels in an external
 * margin, the same design EqChart itself moved away from, for the same reason: `currentColor`
 * reads fine against the page's own (theme-following) background but is a mismatch once the plot
 * sits on a guaranteed-dark screen regardless of theme.
 */

const W = 720;
const MS_STEPS = [0.1, 0.2, 0.5, 1, 2, 5, 10, 20, 50, 100, 200];
const WINDOW_DB = -70; // decay tail below this (rel. peak) is treated as settled
const FLOOR_DB = -80; // display floor
const DB_TICKS = [0, -20, -40, -60];

export function ImpulseChart({
  bands,
  color,
  height = 215,
  fs = FS,
  legendHost,
}: {
  bands: Band[];
  color: string;
  height?: number;
  fs?: number;
  /** Full-width host to portal the caption into (matches EqChart's legend placement). */
  legendHost?: HTMLElement | null;
}) {
  const { t } = useTranslation();
  const H = height;
  // Same thin-buffer-only margins as EqChart's own PAD (see that file's doc): a fixed 10px on the
  // time axis, EQ_V_INSET_FRAC*H on the dB axis — not picked-to-fit values, since both axis labels
  // now draw ON the tube itself instead of reserving external room for them.
  const PAD = { l: 10, r: 10, t: H * EQ_V_INSET_FRAC, b: H * EQ_V_INSET_FRAC };
  const plotW = W - PAD.l - PAD.r;

  const { env, n, peak } = useMemo(() => {
    const CAP = 16384; // ~340 ms at 48 kHz
    const h = impulseResponse(bands, CAP, fs);
    let peak = 1e-12;
    for (const v of h) peak = Math.max(peak, Math.abs(v));
    // Decay window: last sample whose envelope is still above WINDOW_DB of the peak.
    const thr = peak * 10 ** (WINDOW_DB / 20);
    let last = 0;
    for (let i = 0; i < CAP; i++) if (Math.abs(h[i]) >= thr) last = i;
    const n = Math.min(CAP, Math.max(Math.round(0.001 * fs), Math.round(last * 1.15) + 1));
    // Upper envelope: one peak-hold value per output column (clean, no zero-crossing hair).
    const cols = Math.round(plotW);
    const env = new Float64Array(cols);
    for (let c = 0; c < cols; c++) {
      const i0 = Math.floor((c / cols) * n);
      const i1 = Math.max(i0 + 1, Math.floor(((c + 1) / cols) * n));
      let m = 0;
      for (let i = i0; i < i1 && i < n; i++) m = Math.max(m, Math.abs(h[i]));
      env[c] = m;
    }
    return { env, n, peak };
  }, [bands, fs, plotW]);

  const msTotal = (n / fs) * 1000;
  const xCol = (c: number) => PAD.l + (c / (env.length - 1)) * plotW;
  const yDb = (db: number) => PAD.t + (db / FLOOR_DB) * (H - PAD.t - PAD.b);

  let d = "";
  for (let c = 0; c < env.length; c++) {
    const db = Math.max(FLOOR_DB, 20 * Math.log10(Math.max(env[c], 1e-12) / peak));
    d += `${c ? "L" : "M"}${xCol(c).toFixed(2)},${yDb(db).toFixed(2)}`;
  }

  const stepMs = MS_STEPS.find((s) => msTotal / s <= 6) ?? 200;
  const ticks: number[] = [];
  for (let t = 0; t <= msTotal + 1e-6; t += stepMs) ticks.push(+t.toFixed(2));

  return (
    <div className="eq-chart">
      {/* Dark instrument screen, unconditional (no spectrum backdrop here to gate it on, unlike
          EqChart's own — see .eq-chart-screen's doc in App.css). */}
      <div className="eq-chart-screen" aria-hidden="true" />
      {/* Container-relative (`.eq-chart`'s own JS-owned box — see `.chart-wrap`'s doc in App.css),
          not intrinsic `height:auto` — see EqChart.tsx's own SVG for the fuller reasoning, shared
          verbatim since this uses the identical `.eq-chart` wrapper class. */}
      <svg viewBox={`0 0 ${W} ${H}`} style={{ width: "100%", height: "100%" }} role="img" aria-label={t("chart.impulseAria")}>
        {/* dB gridlines + labels, both drawn ON the dark screen now — same accent-tinted-on-dark,
            on-tube convention EqChart's own gridlines use (see that file's doc for the full why). */}
        {DB_TICKS.map((db) => (
          <g key={db}>
            <line x1={PAD.l} x2={W - PAD.r} y1={yDb(db)} y2={yDb(db)} stroke="var(--accent)" strokeOpacity={db === 0 ? 0.4 : 0.16} />
            <text x={PAD.l + 5} y={yDb(db) - 3} textAnchor="start" fontSize="10" fill="var(--accent)" opacity={0.6}>
              {db}
            </text>
          </g>
        ))}
        {/* Time gridlines + labels — same treatment. The 0ms tick always sits exactly on the plot's
            own left edge (the decay always starts at t=0), so it anchors start like EqChart's own
            edge ticks (GRID_HZ's F_MIN/F_MAX) instead of centring, which would overhang the corner;
            later ticks aren't guaranteed to land exactly on the right edge (they stop at whatever
            multiple of `stepMs` is ≤ the auto-sized decay window), so only this one needs it. */}
        {ticks.map((m) => {
          const i = (m / 1000) * fs;
          const c = (i / n) * (env.length - 1);
          const x = xCol(c);
          return (
            <g key={m}>
              <line x1={x} x2={x} y1={PAD.t} y2={H - PAD.b} stroke="var(--accent)" strokeOpacity={0.16} />
              <text x={m === 0 ? x + 3 : x} y={H - PAD.b - 5} textAnchor={m === 0 ? "start" : "middle"} fontSize="10" fill="var(--accent)" opacity={0.6}>
                {m} ms
              </text>
            </g>
          );
        })}
        <path d={d} fill="none" stroke={color} strokeWidth={1.6} strokeLinejoin="round" />
      </svg>
      {legendPortal(
        <div className="eq-legend" style={{ opacity: 0.55, fontSize: "0.72rem" }}>
          <span>{t("chart.impulseCaption")}</span>
        </div>,
        legendHost,
      )}
    </div>
  );
}

/** Render into `host` via a portal when provided, else inline (mirrors EqChart). */
function legendPortal(markup: ReactNode, host?: HTMLElement | null) {
  return host ? createPortal(markup, host) : markup;
}
