import { useMemo } from "react";
import { Band, FS, impulseResponse, logGrid, phaseDeg } from "./biquad";

/**
 * §5.2 "nerd" overlays for the applied filter chain — deliberately off by default and
 * shown in their own strips (a different domain/axis than the magnitude chart, so folding
 * them into it would either need a second axis or misread). Both are pure client-side
 * biquad maths (biquad.ts), so they cost nothing to draw and need no sidecar.
 */

const F_MIN = 20;
const F_MAX = 20000;
const GRID_HZ = [20, 100, 1000, 10000, 20000];
const fmtHz = (f: number) => (f >= 1000 ? `${f / 1000}k` : `${f}`);
const W = 720;
const PAD = { l: 42, r: 12, t: 8, b: 18 };

/** Phase shift the EQ introduces, in degrees, over the log-frequency axis. */
export function PhaseStrip({ bands, color, height = 92 }: { bands: Band[]; color: string; height?: number }) {
  const H = height;
  const { freqs, phase, range } = useMemo(() => {
    const freqs = logGrid(480, F_MIN, F_MAX);
    const phase = phaseDeg(bands, freqs);
    let m = 45;
    for (const p of phase) m = Math.max(m, Math.abs(p));
    return { freqs, phase, range: Math.ceil(m / 45) * 45 }; // symmetric, snapped to 45°
  }, [bands]);

  const lnMin = Math.log(F_MIN);
  const lnSpan = Math.log(F_MAX) - lnMin;
  const x = (f: number) => PAD.l + ((Math.log(f) - lnMin) / lnSpan) * (W - PAD.l - PAD.r);
  const y = (deg: number) => PAD.t + ((range - deg) / (2 * range)) * (H - PAD.t - PAD.b);

  let d = "";
  for (let i = 0; i < freqs.length; i++) d += `${i ? "L" : "M"}${x(freqs[i]).toFixed(2)},${y(phase[i]).toFixed(2)}`;

  return (
    <svg viewBox={`0 0 ${W} ${H}`} style={{ width: "100%", height: "auto" }} role="img" aria-label="Filter phase response">
      {[-range, 0, range].map((t) => (
        <g key={t}>
          <line x1={PAD.l} x2={W - PAD.r} y1={y(t)} y2={y(t)} stroke="currentColor" strokeOpacity={t === 0 ? 0.28 : 0.1} />
          <text x={PAD.l - 5} y={y(t) + 3} textAnchor="end" fontSize="9" fill="currentColor" opacity={0.55}>
            {t > 0 ? `+${t}` : t}°
          </text>
        </g>
      ))}
      {GRID_HZ.map((f) => (
        <g key={f}>
          <line x1={x(f)} x2={x(f)} y1={PAD.t} y2={H - PAD.b} stroke="currentColor" strokeOpacity={0.08} />
          <text x={x(f)} y={H - PAD.b + 11} textAnchor="middle" fontSize="9" fill="currentColor" opacity={0.5}>
            {fmtHz(f)}
          </text>
        </g>
      ))}
      <path d={d} fill="none" stroke={color} strokeWidth={1.6} strokeLinejoin="round" />
    </svg>
  );
}

/** The filter chain's impulse response h[n] over time (its "ring"). */
export function ImpulseStrip({ bands, color, height = 92, fs = FS }: { bands: Band[]; color: string; height?: number; fs?: number }) {
  const H = height;
  const N = 480;
  const { h, peak } = useMemo(() => {
    const h = impulseResponse(bands, N, fs);
    let peak = 1e-6;
    for (const v of h) peak = Math.max(peak, Math.abs(v));
    return { h, peak };
  }, [bands, fs]);

  const msTotal = (N / fs) * 1000;
  const x = (i: number) => PAD.l + (i / (N - 1)) * (W - PAD.l - PAD.r);
  const y = (v: number) => PAD.t + ((peak - v) / (2 * peak)) * (H - PAD.t - PAD.b);

  let d = "";
  for (let i = 0; i < N; i++) d += `${i ? "L" : "M"}${x(i).toFixed(2)},${y(h[i]).toFixed(2)}`;

  const msTicks = [0, 2, 4, 6, 8, 10].filter((m) => m <= msTotal + 0.01);
  return (
    <svg viewBox={`0 0 ${W} ${H}`} style={{ width: "100%", height: "auto" }} role="img" aria-label="Filter impulse response">
      <line x1={PAD.l} x2={W - PAD.r} y1={y(0)} y2={y(0)} stroke="currentColor" strokeOpacity={0.28} />
      <text x={PAD.l - 5} y={y(0) + 3} textAnchor="end" fontSize="9" fill="currentColor" opacity={0.5}>
        0
      </text>
      {msTicks.map((m) => {
        const i = (m / 1000) * fs;
        return (
          <g key={m}>
            <line x1={x(i)} x2={x(i)} y1={PAD.t} y2={H - PAD.b} stroke="currentColor" strokeOpacity={0.08} />
            <text x={x(i)} y={H - PAD.b + 11} textAnchor="middle" fontSize="9" fill="currentColor" opacity={0.5}>
              {m} ms
            </text>
          </g>
        );
      })}
      <path d={d} fill="none" stroke={color} strokeWidth={1.4} strokeLinejoin="round" />
    </svg>
  );
}
