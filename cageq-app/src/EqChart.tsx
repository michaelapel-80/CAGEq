import { useMemo } from "react";
import { Band, composedCurveDb, logGrid } from "./biquad";

/**
 * §5.2 interactive diagram — stage 1a: renders the composed EQ curve.
 *
 * Log frequency axis (20 Hz – 20 kHz, constant ratio per octave, the audio-standard
 * view) against dB. The curve is computed client-side from the band list with AutoEq's
 * exact biquad model (see biquad.ts), so it matches the fit and what EqAPO applies —
 * and so later drag interactions can recompute locally without an IPC round-trip.
 */

const GRID_HZ = [20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000];
const F_MIN = 20;
const F_MAX = 20000;
const fmtHz = (f: number) => (f >= 1000 ? `${f / 1000}k` : `${f}`);

export function EqChart({
  bands,
  color = "#3b82f6",
  height = 210,
}: {
  bands: Band[];
  color?: string;
  height?: number;
}) {
  const W = 720;
  const H = height;
  const PAD = { l: 40, r: 12, t: 12, b: 24 };

  const { freqs, curve, yMin, yMax, step } = useMemo(() => {
    const freqs = logGrid(480, F_MIN, F_MAX);
    const curve = composedCurveDb(bands, freqs);
    let lo = 0;
    let hi = 0;
    for (const v of curve) {
      if (v < lo) lo = v;
      if (v > hi) hi = v;
    }
    // Symmetric range with a sane floor so a flat curve isn't wildly zoomed.
    const span = Math.max(6, Math.ceil(Math.max(Math.abs(lo), Math.abs(hi)) + 1));
    const step = span <= 9 ? 3 : span <= 18 ? 6 : 12;
    return { freqs, curve, yMin: -span, yMax: span, step };
  }, [bands]);

  const lnMin = Math.log(F_MIN);
  const lnSpan = Math.log(F_MAX) - lnMin;
  const x = (f: number) => PAD.l + ((Math.log(f) - lnMin) / lnSpan) * (W - PAD.l - PAD.r);
  const y = (db: number) => PAD.t + ((yMax - db) / (yMax - yMin)) * (H - PAD.t - PAD.b);

  const path = useMemo(() => {
    let d = "";
    for (let i = 0; i < freqs.length; i++) {
      d += `${i ? "L" : "M"}${x(freqs[i]).toFixed(2)},${y(curve[i]).toFixed(2)}`;
    }
    return d;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [freqs, curve, yMin, yMax, H]);

  const dbTicks: number[] = [];
  for (let v = -Math.floor(yMax / step) * step; v <= yMax; v += step) dbTicks.push(v);

  return (
    <svg
      viewBox={`0 0 ${W} ${H}`}
      style={{ width: "100%", height: "auto", userSelect: "none", touchAction: "none" }}
      role="img"
      aria-label="Equalizer response curve"
    >
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

      {/* the composed response */}
      <path d={path} fill="none" stroke={color} strokeWidth={2} strokeLinejoin="round" />
    </svg>
  );
}
