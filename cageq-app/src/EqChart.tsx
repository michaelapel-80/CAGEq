import { useEffect, useMemo, useRef, useState } from "react";
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

/** Draggable band handles: X = centre frequency, Y = gain, wheel = Q (§5.2). */
export type Nodes = {
  bands: Band[];
  color: string;
  onChange: (index: number, patch: Partial<Band>) => void;
  /** Fired once when a drag finishes (for a final, un-throttled commit). */
  onDragEnd?: () => void;
  disabled?: boolean;
};

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));

const GRID_HZ = [20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000];
const F_MIN = 20;
const F_MAX = 20000;
const fmtHz = (f: number) => (f >= 1000 ? `${f / 1000}k` : `${f}`);

export function EqChart({
  series,
  nodes,
  height = 210,
}: {
  series: Series[];
  nodes?: Nodes;
  height?: number;
}) {
  const W = 720;
  const H = height;
  const PAD = { l: 40, r: 12, t: 12, b: 24 };
  const svgRef = useRef<SVGSVGElement>(null);
  const [dragIdx, setDragIdx] = useState<number | null>(null);
  const [hoverIdx, setHoverIdx] = useState<number | null>(null);

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
  // Inverse scales, for turning a pointer position back into (frequency, gain).
  const invX = (vx: number) => Math.exp(lnMin + ((vx - PAD.l) / (W - PAD.l - PAD.r)) * lnSpan);
  const invY = (vy: number) => yMax - ((vy - PAD.t) / (H - PAD.t - PAD.b)) * (yMax - yMin);

  /** Pointer client coords -> viewBox coords (the SVG scales to its container). */
  const toViewBox = (clientX: number, clientY: number) => {
    const r = svgRef.current!.getBoundingClientRect();
    return { vx: ((clientX - r.left) / r.width) * W, vy: ((clientY - r.top) / r.height) * H };
  };

  // Q on the wheel. Registered natively with { passive: false } because React's
  // synthetic wheel handler can't preventDefault — without it the page scrolls too.
  useEffect(() => {
    const el = svgRef.current;
    if (!el || !nodes || nodes.disabled) return;
    const onWheel = (e: WheelEvent) => {
      if (hoverIdx == null) return;
      e.preventDefault();
      const band = nodes.bands[hoverIdx];
      if (!band) return;
      const factor = e.deltaY > 0 ? 1 / 1.12 : 1.12;
      nodes.onChange(hoverIdx, { q: Math.round(clamp(band.q * factor, 0.1, 20) * 100) / 100 });
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, [nodes, hoverIdx]);

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
      ref={svgRef}
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

      {/* draggable band handles (§5.2): X = fc, Y = gain, wheel = Q */}
      {nodes?.bands.map((b, i) => {
        const cx = x(clamp(b.freq_hz, F_MIN, F_MAX));
        const cy = y(clamp(b.gain_db, yMin, yMax));
        const active = dragIdx === i || hoverIdx === i;
        return (
          <g key={`n${i}`}>
            <circle
              cx={cx}
              cy={cy}
              r={active ? 7 : 5.5}
              fill={nodes.color}
              fillOpacity={active ? 0.95 : 0.75}
              stroke="currentColor"
              strokeOpacity={0.35}
              style={{ cursor: nodes.disabled ? "default" : dragIdx === i ? "grabbing" : "grab" }}
              onPointerEnter={() => setHoverIdx(i)}
              onPointerLeave={() => setHoverIdx((h) => (h === i ? null : h))}
              onPointerDown={(e) => {
                if (nodes.disabled) return;
                // Without preventDefault the browser's native selection drag races ours.
                e.preventDefault();
                (e.target as Element).setPointerCapture(e.pointerId);
                setDragIdx(i);
              }}
              onPointerMove={(e) => {
                if (dragIdx !== i) return;
                const { vx, vy } = toViewBox(e.clientX, e.clientY);
                nodes.onChange(i, {
                  freq_hz: Math.round(clamp(invX(vx), F_MIN, F_MAX)),
                  gain_db: Math.round(clamp(invY(vy), -20, 20) * 10) / 10,
                });
              }}
              onPointerUp={(e) => {
                if (dragIdx !== i) return;
                (e.target as Element).releasePointerCapture(e.pointerId);
                setDragIdx(null);
                nodes.onDragEnd?.();
              }}
            />
            {active && (
              <text x={cx} y={cy - 11} textAnchor="middle" fontSize="9.5" fill="currentColor" opacity={0.8}>
                {b.freq_hz >= 1000 ? `${(b.freq_hz / 1000).toFixed(2)}k` : b.freq_hz} Hz · {b.gain_db > 0 ? "+" : ""}
                {b.gain_db.toFixed(1)} dB · Q {b.q.toFixed(2)}
              </text>
            )}
          </g>
        );
      })}

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
