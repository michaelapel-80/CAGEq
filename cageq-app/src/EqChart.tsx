import { useEffect, useId, useMemo, useRef, useState } from "react";
import { Band, composedCurveDb, logGrid, phaseDeg } from "./biquad";

/**
 * §5.2 interactive diagram.
 *
 * Log frequency axis (20 Hz – 20 kHz, constant ratio per octave, the audio-standard
 * view) against dB. Curves are computed client-side from band lists with AutoEq's
 * exact biquad model (see biquad.ts), so they match the fit and what EqAPO applies —
 * and so drag interactions can recompute locally without an IPC round-trip.
 *
 * Takes a list of series (curves) plus optional markers (fixed band handles, e.g. the
 * AutoEq fit shown as diamonds) so it can overlay the editable tone layer, the applied
 * total, and the other comparison slots (Dry/A/B) at once. Any series or marker set can
 * be toggled from the legend — a click hides it, so a busy overlay stays legible.
 */

export type Series = {
  /** Stable identity for the legend toggle and React keys. */
  id: string;
  bands: Band[];
  color: string;
  label: string;
  /** Dimmed, thinner context line (not the layer being edited / not the active slot). */
  muted?: boolean;
  /** Start hidden (user can reveal via the legend) — for opt-in "nerd" layers. */
  defaultHidden?: boolean;
};

/** A fixed (non-draggable) set of band handles drawn as diamonds — e.g. the AutoEq fit. */
export type Marker = {
  id: string;
  bands: Band[];
  color: string;
  label: string;
  defaultHidden?: boolean;
};

/**
 * A pre-sampled reference curve given as raw (frequency, dB) points rather than bands —
 * e.g. AutoEq's ideal correction, which isn't a biquad cascade. Drawn as a thin dotted
 * line so it reads as context the fitted curve is chasing, not another EQ layer.
 */
export type RefCurve = {
  id: string;
  points: { f: number; db: number }[];
  color: string;
  label: string;
  defaultHidden?: boolean;
};

/** The filter chain's phase response, drawn on a **secondary right axis** (degrees) — a
 *  Bode-style overlay, off by default. Computed from bands, so it tracks live edits. */
export type PhaseCurve = { id: string; bands: Band[]; color: string; label: string; defaultHidden?: boolean };

/** Draggable band handles: X = centre frequency, Y = gain, wheel = Q (§5.2). */
export type Nodes = {
  bands: Band[];
  color: string;
  onChange: (index: number, patch: Partial<Band>) => void;
  /** Fired once when a drag finishes (for a final, un-throttled commit). */
  onDragEnd?: () => void;
  /** Double-click empty plot space to create a band there (x → fc, y → gain). */
  onAdd?: (freq_hz: number, gain_db: number) => void;
  /** Double-click a node to remove that band (the symmetric gesture to onAdd). */
  onRemove?: (index: number) => void;
  /** Index of a just-added band to pulse-highlight so the eye catches it. */
  highlightIdx?: number;
  disabled?: boolean;
};

const clamp = (v: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, v));

const GRID_HZ = [20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000];
const F_MIN = 20;
const F_MAX = 20000;
// Reference curves (measured raw / target) only count toward the Y auto-scale up to this
// frequency: they diverge sharply in the top octaves (measurement noise + treble roll-off),
// which would otherwise blow the scale out to ±24 dB. Still drawn full-range (then clipped).
const RANGE_F_MAX = 12000;
const fmtHz = (f: number) => (f >= 1000 ? `${f / 1000}k` : `${f}`);

export function EqChart({
  series,
  markers = [],
  refs = [],
  phase,
  nodes,
  height = 210,
}: {
  series: Series[];
  markers?: Marker[];
  refs?: RefCurve[];
  phase?: PhaseCurve;
  nodes?: Nodes;
  height?: number;
}) {
  const W = 720;
  const H = height;
  const svgRef = useRef<SVGSVGElement>(null);
  const clipId = useId(); // clips the plotted curves to the plot rect (see refPaths)
  // Manual double-click detection from bubbled `click` events. The native `dblclick` is
  // unreliable here: pointer-capture/preventDefault on a node disrupts its synthesis, and on
  // empty space the two clicks often land on *different* thin children (a gridline vs a
  // curve), so the browser fires no dblclick at all. Bubbled clicks always reach the SVG.
  // Records the previous click's time, *screen* position (so the tolerance is a real pixel
  // distance regardless of how wide the 720-unit viewBox is drawn), and what it was over
  // (node index or null) — the second click must match to count as that gesture's double.
  const lastTap = useRef<{ t: number; x: number; y: number; overIdx: number | null } | null>(null);
  const DBL_MS = 450;
  const DBL_DIST = 16; // screen px — did the pointer barely move between the two clicks?
  const [dragIdx, setDragIdx] = useState<number | null>(null);
  const [hoverIdx, setHoverIdx] = useState<number | null>(null);
  // Legend visibility: `overrides` holds only ids the user has clicked; everything else
  // falls back to its `defaultHidden` prop (so opt-in "nerd" layers start off, and a fresh
  // series obeys its default). Absent from both = visible.
  const [overrides, setOverrides] = useState<Record<string, boolean>>({});
  const defaultHidden = useMemo(() => {
    const m: Record<string, boolean> = {};
    for (const s of series) if (s.defaultHidden) m[s.id] = true;
    for (const r of refs) if (r.defaultHidden) m[r.id] = true;
    for (const mk of markers) if (mk.defaultHidden) m[mk.id] = true;
    if (phase?.defaultHidden) m[phase.id] = true;
    return m;
  }, [series, refs, markers, phase]);
  const isHidden = (id: string) => (id in overrides ? overrides[id] : defaultHidden[id] === true);
  const toggle = (id: string) => setOverrides((o) => ({ ...o, [id]: !isHidden(id) }));

  // The phase overlay needs a right axis (degrees) — reserve room for its labels only when
  // it's actually shown, so the plot doesn't lose width when it isn't.
  const phaseOn = !!phase && !isHidden(phase.id);
  const PAD = { l: 40, r: phaseOn ? 34 : 12, t: 12, b: 24 };

  const { freqs, curves, yMin, yMax, step } = useMemo(() => {
    const freqs = logGrid(480, F_MIN, F_MAX);
    const curves = series.map((s) => composedCurveDb(s.bands, freqs));
    let lo = 0;
    let hi = 0;
    // Auto-range over visible curves and visible marker gains only (hiding a spiky layer
    // lets the rest breathe). The user's own EQ (series) and the fit (markers) use the full
    // range — a deliberate HF boost should be visible. Reference curves (measured raw /
    // target), though, diverge noisily in the top octaves, so those only count up to
    // RANGE_F_MAX; the rest of each ref is still drawn, just clipped to the frame.
    series.forEach((s, i) => {
      if (isHidden(s.id)) return;
      for (const v of curves[i]) {
        if (v < lo) lo = v;
        if (v > hi) hi = v;
      }
    });
    for (const m of markers) {
      if (isHidden(m.id)) continue;
      for (const b of m.bands) {
        if (b.gain_db < lo) lo = b.gain_db;
        if (b.gain_db > hi) hi = b.gain_db;
      }
    }
    for (const rc of refs) {
      if (isHidden(rc.id)) continue;
      for (const p of rc.points) {
        if (p.f > RANGE_F_MAX) continue;
        if (p.db < lo) lo = p.db;
        if (p.db > hi) hi = p.db;
      }
    }
    // Symmetric range with a sane floor so a flat curve isn't wildly zoomed.
    const span = Math.max(6, Math.ceil(Math.max(Math.abs(lo), Math.abs(hi)) + 1));
    const step = span <= 9 ? 3 : span <= 18 ? 6 : 12;
    return { freqs, curves, yMin: -span, yMax: span, step };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [series, markers, refs, overrides, defaultHidden]);

  // Phase curve on the secondary axis (computed on the same freq grid). Its degrees range
  // is symmetric and snapped to 45°, independent of the dB axis.
  const { phaseCurve, phaseRange } = useMemo(() => {
    if (!phase) return { phaseCurve: null as Float64Array | null, phaseRange: 90 };
    const p = phaseDeg(phase.bands, freqs);
    let m = 45;
    for (const v of p) m = Math.max(m, Math.abs(v));
    return { phaseCurve: p, phaseRange: Math.ceil(m / 45) * 45 };
  }, [phase, freqs]);

  const lnMin = Math.log(F_MIN);
  const lnSpan = Math.log(F_MAX) - lnMin;
  const x = (f: number) => PAD.l + ((Math.log(f) - lnMin) / lnSpan) * (W - PAD.l - PAD.r);
  const y = (db: number) => PAD.t + ((yMax - db) / (yMax - yMin)) * (H - PAD.t - PAD.b);
  const yPhase = (deg: number) => PAD.t + ((phaseRange - deg) / (2 * phaseRange)) * (H - PAD.t - PAD.b);
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

  // Reference curves come as raw (f, db) points. Use *real* y values (not clamped to the
  // range) and let the SVG clip mask hide the out-of-plot part — clamping would draw a
  // bogus flat line pinned to the top/bottom edge instead of the line exiting the frame.
  const refPaths = refs.map((rc) => {
    let d = "";
    rc.points.forEach((p, i) => {
      const px = x(clamp(p.f, F_MIN, F_MAX));
      const py = y(p.db);
      d += `${i ? "L" : "M"}${px.toFixed(2)},${py.toFixed(2)}`;
    });
    return d;
  });

  const phasePath = useMemo(() => {
    if (!phaseCurve) return "";
    let d = "";
    for (let i = 0; i < freqs.length; i++) d += `${i ? "L" : "M"}${x(freqs[i]).toFixed(2)},${yPhase(phaseCurve[i]).toFixed(2)}`;
    return d;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [phaseCurve, freqs, phaseRange, PAD.r, H]);

  const dbTicks: number[] = [];
  for (let v = -Math.floor(yMax / step) * step; v <= yMax; v += step) dbTicks.push(v);

  // Legend rows: every series, then reference curves, phase, then markers, in draw order.
  type LegendStyle = "solid" | "dashed" | "dotted" | "diamond";
  const legend: { id: string; color: string; label: string; style: LegendStyle }[] = [
    ...series.map((s) => ({ id: s.id, color: s.color, label: s.label, style: (s.muted ? "dashed" : "solid") as LegendStyle })),
    ...refs.map((rc) => ({ id: rc.id, color: rc.color, label: rc.label, style: "dotted" as LegendStyle })),
    ...(phase ? [{ id: phase.id, color: phase.color, label: phase.label, style: "dashed" as LegendStyle }] : []),
    ...markers.map((m) => ({ id: m.id, color: m.color, label: m.label, style: "diamond" as LegendStyle })),
  ];

  return (
    <div className="eq-chart">
    <svg
      ref={svgRef}
      viewBox={`0 0 ${W} ${H}`}
      style={{ width: "100%", height: "auto", userSelect: "none", touchAction: "none" }}
      role="img"
      aria-label="Equalizer response curve"
      onClick={(e) => {
        // Symmetric double-click gestures (detected manually — see lastTap): a second click
        // close to the first (time + screen distance) *and over the same target* → over that
        // node, remove it; over empty space, create a band at the cursor (x → fc, y → gain).
        // The same-target gate stops an empty click then a nearby node click (or vice versa)
        // from being misread as a delete/add.
        if (!nodes || nodes.disabled) return;
        const prev = lastTap.current;
        const isDouble = prev != null && e.timeStamp - prev.t < DBL_MS && Math.hypot(e.clientX - prev.x, e.clientY - prev.y) < DBL_DIST;
        if (isDouble && hoverIdx != null && prev!.overIdx === hoverIdx) {
          lastTap.current = null; // consume, so a third click doesn't immediately re-fire
          nodes.onRemove?.(hoverIdx);
          setHoverIdx(null); // the hovered index is about to shift under us
          return;
        }
        if (isDouble && hoverIdx == null && prev!.overIdx == null && nodes.onAdd) {
          lastTap.current = null;
          const { vx, vy } = toViewBox(e.clientX, e.clientY);
          if (vx >= PAD.l && vx <= W - PAD.r && vy >= PAD.t && vy <= H - PAD.b) {
            nodes.onAdd(Math.round(clamp(invX(vx), F_MIN, F_MAX)), Math.round(clamp(invY(vy), -20, 20) * 10) / 10);
          }
          return;
        }
        // First click, timed out, moved too far, or a cross-gesture pair → (re)start from here.
        lastTap.current = { t: e.timeStamp, x: e.clientX, y: e.clientY, overIdx: hoverIdx };
      }}
    >
      <defs>
        <clipPath id={clipId}>
          <rect x={PAD.l} y={PAD.t} width={W - PAD.l - PAD.r} height={H - PAD.t - PAD.b} />
        </clipPath>
      </defs>
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

      {/* secondary phase axis (degrees), right side — only when the phase overlay is shown */}
      {phaseOn && phase && (
        <g>
          {[phaseRange, 0, -phaseRange].map((deg) => (
            <text key={deg} x={W - PAD.r + 4} y={yPhase(deg) + 3.5} textAnchor="start" fontSize="10" fill={phase.color} opacity={0.85}>
              {deg > 0 ? `+${deg}` : deg}°
            </text>
          ))}
        </g>
      )}

      {/* plotted curves/refs/phase/markers, clipped to the plot rect so out-of-range parts
          leave the frame instead of flattening against the edge */}
      <g clipPath={`url(#${clipId})`}>
      {/* curves — muted context lines first so the edited/active layer draws on top */}
      {series.map((s, i) =>
        s.muted && !isHidden(s.id) ? (
          <path
            key={`c${s.id}`}
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
        !s.muted && !isHidden(s.id) ? (
          <path key={`c${s.id}`} d={paths[i]} fill="none" stroke={s.color} strokeWidth={2} strokeLinejoin="round" />
        ) : null,
      )}

      {/* reference curves (e.g. AutoEq's ideal correction) — thin dotted context */}
      {refs.map((rc, i) =>
        isHidden(rc.id) ? null : (
          <path
            key={`r${rc.id}`}
            d={refPaths[i]}
            fill="none"
            stroke={rc.color}
            strokeWidth={1.4}
            strokeOpacity={0.7}
            strokeDasharray="1 3"
            strokeLinecap="round"
            strokeLinejoin="round"
          />
        ),
      )}

      {/* phase overlay (right axis) — dashed so it doesn't read as a magnitude curve */}
      {phaseOn && phasePath && (
        <path d={phasePath} fill="none" stroke={phase!.color} strokeWidth={1.5} strokeOpacity={0.85} strokeDasharray="6 3" strokeLinejoin="round" />
      )}

      {/* fixed band markers (e.g. the AutoEq fit) as small diamonds */}
      {markers.map((m) =>
        isHidden(m.id)
          ? null
          : m.bands.map((b, i) => {
              const cx = x(clamp(b.freq_hz, F_MIN, F_MAX));
              const cy = y(b.gain_db); // real y; the clip hides an out-of-range diamond
              const r = 3.4;
              return (
                <path
                  key={`m${m.id}-${i}`}
                  d={`M${cx},${cy - r}L${cx + r},${cy}L${cx},${cy + r}L${cx - r},${cy}Z`}
                  fill={m.color}
                  fillOpacity={0.7}
                  stroke="currentColor"
                  strokeOpacity={0.25}
                  style={{ pointerEvents: "none" }}
                />
              );
            }),
      )}
      </g>

      {/* draggable band handles (§5.2): X = fc, Y = gain, wheel = Q. Hidden entirely when
          disabled (e.g. Dry active) — stale handles from the last slot shouldn't linger. */}
      {nodes && !nodes.disabled && nodes.bands.map((b, i) => {
        const cx = x(clamp(b.freq_hz, F_MIN, F_MAX));
        const cy = y(clamp(b.gain_db, yMin, yMax));
        const active = dragIdx === i || hoverIdx === i;
        const isNew = nodes.highlightIdx === i;
        // Fixed macro bands (Bass/Treble/Air) ride along at runtime even though the type is
        // Band; they can't be removed, so don't promise it in the tooltip.
        const isFixed = (b as { fixed?: boolean }).fixed === true;
        return (
          <g key={`n${i}`}>
            <title>{isFixed ? "Macro band — drag to edit · wheel for Q (not removable)" : "Drag to edit · wheel for Q · double-click to remove"}</title>
            {isNew && (
              <circle cx={cx} cy={cy} r={6} fill="none" stroke={nodes.color} strokeWidth={2} className="eq-node-pulse" />
            )}
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
                // A missed pointerup would otherwise drag the node on plain hover; if no
                // button is held, end the drag instead.
                if (e.buttons === 0) {
                  try {
                    (e.target as Element).releasePointerCapture(e.pointerId);
                  } catch {
                    /* capture may already be gone */
                  }
                  setDragIdx(null);
                  nodes.onDragEnd?.();
                  return;
                }
                const { vx, vy } = toViewBox(e.clientX, e.clientY);
                nodes.onChange(i, {
                  freq_hz: Math.round(clamp(invX(vx), F_MIN, F_MAX)),
                  gain_db: Math.round(clamp(invY(vy), -20, 20) * 10) / 10,
                });
              }}
              onPointerUp={(e) => {
                if (dragIdx !== i) return;
                try {
                  (e.target as Element).releasePointerCapture(e.pointerId);
                } catch {
                  /* capture may already be gone */
                }
                setDragIdx(null);
                nodes.onDragEnd?.();
              }}
              onPointerCancel={() => {
                if (dragIdx !== i) return;
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

    </svg>

      {/* legend — below the plot (not overlapping it); click a chip to hide/show */}
      <div className="eq-legend">
        {legend.map((row) => {
          const off = isHidden(row.id);
          return (
            <button
              type="button"
              key={`l${row.id}`}
              className={`eq-legend-item${off ? " off" : ""}`}
              aria-pressed={!off}
              onClick={() => toggle(row.id)}
              title={off ? `Show ${row.label}` : `Hide ${row.label}`}
            >
              <svg className="eq-swatch" viewBox="0 0 18 10" width="18" height="10" aria-hidden="true">
                {row.style === "diamond" ? (
                  <path d="M9,1.5L13,5L9,8.5L5,5Z" fill={row.color} fillOpacity={0.8} />
                ) : (
                  <line
                    x1={1}
                    x2={17}
                    y1={5}
                    y2={5}
                    stroke={row.color}
                    strokeWidth={row.style === "solid" ? 2.4 : row.style === "dotted" ? 1.6 : 1.6}
                    strokeDasharray={row.style === "dotted" ? "1.5 2.5" : row.style === "dashed" ? "4 3" : undefined}
                    strokeLinecap={row.style === "dotted" ? "round" : undefined}
                  />
                )}
              </svg>
              <span>{row.label}</span>
            </button>
          );
        })}
      </div>
    </div>
  );
}
