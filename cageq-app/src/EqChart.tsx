import { useMemo } from "react";
import { Band, composedCurveDb, logGrid } from "./biquad";

/**
 * §5.2 interactive diagram.
 *
 * Log frequency axis (20 Hz – 20 kHz, constant ratio per octave, the audio-standard
 * view) against dB. Curves are computed client-side from band lists with AutoEq's
 * exact biquad model (see biquad.ts), so they match the fit and what EqAPO applies —
 * and so drag interactions can recompute locally without an IPC round-trip.
 *
 * Takes a list of series so it can show the editable **tone** layer against a flat
 * baseline plus the applied total as dimmed context (and, later, Dry/A/B overlays).
 */

export type Series = {
  bands: Band[];
  color: string;
  label: string;
  /** Dimmed, thinner context line (not the layer being edited). */
  muted?: boolean;
};

const GRID_HZ = [20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000];
const F_MIN = 20;
const F_MAX = 20000;
const fmtHz = (f: number) => (f >= 1000 ? `${f / 1000}k` : `${f}`);

export function EqChart({ series, height = 210 }: { series: Series[]; height?: number }) {
  const W = 720;
  const H = height;
  const PAD = { l: 40, r: 12, t: 12, b: 24 };

  const { freqs, curves, yMin, yMax, step } = useMemo(() => {
    const freqs = logGrid(480, F_MIN, F_MAX);
    const curves = series.map((s) => composedCurveDb(s.bands, freqs));
    let lo = 0;
    let hi = 0;
    for (const c of curves) {
      for (const v of c) {
        if (v < lo) lo = v;
        if (v > hi) hi = v;
      }
    }
    // Symmetric range with a sane floor so a flat curve isn't wildly zoomed.
    const span = Math.max(6, Math.ceil(Math.max(Math.abs(lo), Math.abs(hi)) + 1));
    const step = span <= 9 ? 3 : span <= 18 ? 6 : 12;
    return { freqs, curves, yMin: -span, yMax: span, step };
  }, [series]);

  const lnMin = Math.log(F_MIN);
  const lnSpan = Math.log(F_MAX) - lnMin;
  const x = (f: number) => PAD.l + ((Math.log(f) - lnMin) / lnSpan) * (W - PAD.l - PAD.r);
  const y = (db: number) => PAD.t + ((yMax - db) / (yMax - yMin)) * (H - PAD.t - PAD.b);

  const paths = useMemo(
    () =>
      curves.map((curve) => {
        let d = "";
        for (let i = 0; i < freqs.length; i++) {
          d += `${i ? "L" : "M"}${x(freqs[i]).toFixed(2)},${y(curve[i]).toFixed(2)}`;
        }
        return d;
      }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [freqs, curves, yMin, yMax, H],
  );

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

      {/* curves — muted context lines first so the edited layer draws on top */}
      {series.map((s, i) =>
        s.muted ? (
          <path
            key={`c${i}`}
            d={paths[i]}
            fill="none"
            stroke={s.color}
            strokeWidth={1.25}
            strokeOpacity={0.45}
            strokeDasharray="4 3"
            strokeLinejoin="round"
          />
        ) : null,
      )}
      {series.map((s, i) =>
        s.muted ? null : (
          <path key={`c${i}`} d={paths[i]} fill="none" stroke={s.color} strokeWidth={2} strokeLinejoin="round" />
        ),
      )}

      {/* legend */}
      {series.map((s, i) => (
        <g key={`l${i}`} transform={`translate(${PAD.l + 6}, ${PAD.t + 12 + i * 13})`}>
          <line x1={0} x2={16} y1={-3.5} y2={-3.5} stroke={s.color} strokeWidth={s.muted ? 1.25 : 2} strokeOpacity={s.muted ? 0.45 : 1} strokeDasharray={s.muted ? "4 3" : undefined} />
          <text x={21} y={0} fontSize="10" fill="currentColor" opacity={s.muted ? 0.5 : 0.75}>
            {s.label}
          </text>
        </g>
      ))}
    </svg>
  );
}
