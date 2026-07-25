import { useMemo } from "react";
import { Band, FS, impulseResponse } from "./biquad";

/**
 * §5.2 impulse-response view — the applied filter chain's decay, shown *in place of* the
 * magnitude chart (a view swap, not an extra strip, so it costs no vertical space). Pure
 * client-side biquad maths (biquad.ts).
 *
 * On a **dB envelope** axis, not linear: an EQ's impulse is dominated by the n=0 spike, so
 * a resonance's ring sits tens of dB below it and is invisible on a linear plot. In dB the
 * ring's decay is visible, and the time window auto-sets to where the envelope falls below
 * a floor — so a high-Q boost frames its long ring and a broad shelf its quick settle.
 */

const W = 720;
const PAD = { l: 44, r: 14, t: 12, b: 26 };
const MS_STEPS = [0.1, 0.2, 0.5, 1, 2, 5, 10, 20, 50, 100, 200];
const WINDOW_DB = -70; // decay tail below this (rel. peak) is treated as settled
const FLOOR_DB = -80; // display floor
const DB_TICKS = [0, -20, -40, -60];

export function ImpulseChart({ bands, color, height = 215, fs = FS }: { bands: Band[]; color: string; height?: number; fs?: number }) {
  const H = height;
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
      <svg viewBox={`0 0 ${W} ${H}`} style={{ width: "100%", height: "auto" }} role="img" aria-label="Filter impulse-response decay">
        {DB_TICKS.map((db) => (
          <g key={db}>
            <line x1={PAD.l} x2={W - PAD.r} y1={yDb(db)} y2={yDb(db)} stroke="currentColor" strokeOpacity={db === 0 ? 0.28 : 0.1} />
            <text x={PAD.l - 6} y={yDb(db) + 3.5} textAnchor="end" fontSize="10" fill="currentColor" opacity={0.55}>
              {db}
            </text>
          </g>
        ))}
        {ticks.map((m) => {
          const i = (m / 1000) * fs;
          const c = (i / n) * (env.length - 1);
          return (
            <g key={m}>
              <line x1={xCol(c)} x2={xCol(c)} y1={PAD.t} y2={H - PAD.b} stroke="currentColor" strokeOpacity={0.08} />
              <text x={xCol(c)} y={H - PAD.b + 14} textAnchor="middle" fontSize="10" fill="currentColor" opacity={0.55}>
                {m} ms
              </text>
            </g>
          );
        })}
        <path d={d} fill="none" stroke={color} strokeWidth={1.6} strokeLinejoin="round" />
      </svg>
      <div className="eq-legend" style={{ opacity: 0.55, fontSize: "0.72rem" }}>
        <span>Impulse-response decay of the applied filter chain (dB envelope) — window auto-set to the decay.</span>
      </div>
    </div>
  );
}
