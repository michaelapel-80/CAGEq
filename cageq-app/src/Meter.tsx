import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke } from "@tauri-apps/api/core";
import { meterStream, scopeStream, type MeterData } from "./streams";
import type { ScopeData } from "./Vectorscope";
import { createPhosphor } from "./phosphor";
import { useTunableParams } from "./useTunableParams";

/**
 * §5.3c post-EQ output meter, laid out vertically to sit beside the chart (no vertical growth, so
 * it can be on by default). Subscribes to the backend's `monitor` events (WASAPI loopback on the
 * selected render endpoint — already post-EQ) and shows two bars:
 *   • Level (dBFS): a WebGL phosphor beam (see below) + peak (amber) and true-RMS (neutral) marks.
 *   • LUFS: BS.1770 momentary fill + short-term mark — makes the auto-LUFS preamp match visible.
 * Independent of the custom-APO work. Mounting starts capture, unmounting stops it.
 */

// Payload shape (`MeterData`, mirroring cageq-monitor's MeterUpdate) lives in streams.ts, which
// owns the channel-based transport — see there for why channels, not `listen` events.

// Level bar scale (dBFS). Must match the backend's own range (BAR_MIN_DB) so the peak/RMS marks
// line up with the beam.
const LEVEL_MIN_DB = -40;
// LUFS bar scale — momentary/short-term music loudness lives comfortably inside this.
const LUFS_MIN = -40;
const pctOf = (v: number, min: number) => Math.max(0, Math.min(100, ((v - min) / -min) * 100));
const fmt = (v: number) => (v > 0 ? "+" : "") + v.toFixed(1);
// The bars update every event (~60 fps); the numeric readouts refresh slower so digits are readable.
const NUMS_INTERVAL_MS = 200;
// A vertical marker's `bottom`, clamped so a 2px line at the top edge isn't cropped.
const mark = (pct: number) => `clamp(0px, ${pct}%, calc(100% - 2px))`;
// Extra top inset (px) for the bars so they clear the Apply button above the column without
// pushing the layout down. The bars just get shorter; their bottom stays on the chart's X axis.
const BAR_TOP_GAP = 14;

// Level bar: Vectorscope's own connected, velocity-bucketed beam trace (see Vectorscope.tsx's
// mechanism doc), collapsed from its 2D X-Y goniometer down to 1D — X pinned at the bar's centre (a
// thick stroke fills the width on its own), only Y (the sample's instantaneous dB height) varies.
// Worked out and calibrated in cageq-app/spike/meter.html before landing here — see that file's
// header for the full history (three revisions: wrong comparison, no connected beam, velocity
// measured in the wrong domain, a tau/glow mismatch) and for the direct verification that the
// initially-stripy look was a synthetic-test-tone artifact (a single perfectly periodic wave
// revisiting the same discrete heights every cycle), not a flaw in this mechanism.
//
// Live-tunable (on-screen panel, same `useTunableParams` persistence the scopes use) rather than
// hardcoded: deliberately NOT shared with Vectorscope's own tuned Params/defaults even though it's
// the identical mechanism — this panel runs at audio-sample rate against a much denser trace than
// Vectorscope's 2D per-hop batches, so a tau/glow calibrated for one badly miscalibrates the other.
type Params = {
  trailTau: number; // phosphor decay time constant (s)
  tail: number; // multiplies trailTau for *faint* content only — the long afterglow (see phosphor.ts)
  glow: number; // beam brightness at full (slow-beam) intensity; velocity glow dims it from here
  focus: number; // velocity-glow reference — see Vectorscope's own `focus` doc
  beamWidth: number; // fraction of the bar's own (backing-store) width the stroke fills
};
const DEFAULTS: Params = { trailTau: 0.06, tail: 12, glow: 0.06, focus: 8, beamWidth: 0.85 };
const VEL_BUCKETS = 16;
const VEL_FLOOR = 0.05;
const VEL_REF_RATE = 48000;
// Linear amplitude has no natural "pixels" — this is the arbitrary (but fixed, so `focus` stays a
// meaningful, comparable knob) convention: a full -1..1 swing spans one bar-height's worth of
// "distance" for velocity purposes, independent of the dB mapping used for the Y position below and
// independent of the canvas's own resolution (unlike Vectorscope's pixel-space velocity, which does
// scale with its tube size).
const LIN_DIST_SCALE = 420;
// Velocity MUST be measured in the signal's own linear domain, not after the dB mapping below — dB
// compresses/distorts velocity (a fast linear zero-crossing becomes an even faster dB swing, log
// diverging near zero), which was a real, confirmed bug in the spike before this was fixed.
const toDbLin = (lin: number) => (lin > 0 ? 20 * Math.log10(lin) : -Infinity);

/** Parse a `#rrggbb` hex (the `--accent` CSS var) to [r,g,b]; a green phosphor fallback — the same
 *  small helper Vectorscope.tsx keeps privately for the identical purpose. */
function parseAccent(hex: string): [number, number, number] {
  const m = hex.trim().match(/^#?([0-9a-f]{6})$/i);
  if (!m) return [80, 220, 140];
  const n = parseInt(m[1], 16);
  return [(n >> 16) & 255, (n >> 8) & 255, n & 255];
}

export function Meter({
  deviceId,
  plotBox,
  onSampleRate,
}: {
  deviceId: string;
  /** Rendered chart plot-area box (px) so the bars match the chart's Y extent (top gridline → X
   *  axis) instead of stretching past it. Null until measured → bars just fill the column. */
  plotBox: { top: number; height: number } | null;
  /** Reports the endpoint's mix sample rate (Hz, null when unknown) — the header shows it. Called
   *  only when the value changes, so it never re-renders the parent at the meter's 60 fps. */
  onSampleRate?: (hz: number | null) => void;
}) {
  const { t } = useTranslation();
  const [bar, setBar] = useState<MeterData | null>(null); // fast: bars + marks
  const [nums, setNums] = useState<MeterData | null>(null); // throttled: readouts
  const [err, setErr] = useState<string | null>(null);
  const lastNums = useRef(0);
  const lastRate = useRef<number | null>(null);
  const { params, setParams, saveAsDefault, resetToFactory } = useTunableParams("cageq-meter-params", DEFAULTS);
  const [tuning, setTuning] = useState(false);
  const paramsRef = useRef(params);
  paramsRef.current = params;

  useEffect(() => {
    lastRate.current = null; // a new session may report a different (or the same) rate — re-emit it
    const unsub = meterStream.subscribe((update) => {
      setBar(update);
      const rate = update.sample_rate || null;
      if (rate !== lastRate.current) {
        lastRate.current = rate;
        onSampleRate?.(rate);
      }
      const now = performance.now();
      if (now - lastNums.current >= NUMS_INTERVAL_MS) {
        lastNums.current = now;
        setNums(update);
      }
    });
    (async () => {
      try {
        await invoke("start_monitor", { device: deviceId || null });
        setErr(null);
      } catch (e) {
        setErr(String(e));
      }
    })();
    return () => {
      unsub();
      onSampleRate?.(null); // monitor is stopping — the header rate is no longer live
      invoke("stop_monitor").catch(() => {});
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [deviceId]);

  // Level bar's beam canvas — sized by measuring its own container, matching Vectorscope's own
  // ResizeObserver + devicePixelRatio convention (the bar's height is dynamic, driven by `plotBox`
  // above, so a fixed canvas size isn't an option).
  const levelBarRef = useRef<HTMLDivElement | null>(null);
  const beamCanvasRef = useRef<HTMLCanvasElement | null>(null);
  const [beamSize, setBeamSize] = useState({ w: 34, h: 100 });
  useEffect(() => {
    const el = levelBarRef.current;
    if (!el) return;
    const dpr = Math.min(typeof window !== "undefined" ? window.devicePixelRatio || 1 : 1, 2);
    const ro = new ResizeObserver(() => {
      const r = el.getBoundingClientRect();
      setBeamSize({ w: Math.max(1, Math.round(r.width * dpr)), h: Math.max(1, Math.round(r.height * dpr)) });
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  // Latest raw scope payload, written by the listener below and read by the beam's rAF loop — a
  // ref, not state, so the stream drives the imperative canvas without ever re-rendering React.
  const scopeRef = useRef<ScopeData | null>(null);
  useEffect(() => {
    let active = true;
    const unsub = scopeStream.subscribe((s) => {
      if (active) scopeRef.current = s;
    });
    return () => {
      active = false;
      unsub();
    };
  }, []);

  // Register as a scope "viewer" — the same shared counter TimeScope/Vectorscope use (see
  // cageq-monitor's ScopeViewers doc) so the loopback accumulates and emits the raw `xy` stream
  // continuously while this meter is mounted, which is always (this component is "always on" per
  // the doc above) rather than only while an actual scope view happens to be open.
  useEffect(() => {
    void invoke("set_scope_viewer", { active: true });
    return () => void invoke("set_scope_viewer", { active: false });
  }, []);

  // The beam: this frame's raw samples are drawn into a scratch 2D canvas and handed to the
  // phosphor accumulator, which owns the decay and additive composite (see phosphor.ts).
  useEffect(() => {
    const cv = beamCanvasRef.current;
    if (!cv) return;
    const phos = createPhosphor(cv);
    if (!phos) return;
    // See Vectorscope.tsx's identical log — diagnosing a machine-specific "one view reads darker
    // than the others" report by checking whether any view silently fell back off the GPU
    // half-float accumulator (phosphor.ts's `precise`).
    if (!phos.precise) console.warn("[Meter] phosphor fell back to the 8-bit canvas accumulator (no half-float GPU support)");
    const [ar, ag, ab] = parseAccent(getComputedStyle(cv).getPropertyValue("--accent"));

    let raf = 0;
    let last = performance.now();
    let drawn: ScopeData | null = null; // last payload already traced (draw each once)

    const render = () => {
      const now = performance.now();
      const dt = Math.min(0.1, (now - last) / 1000); // clamp after a tab-switch stall
      last = now;
      const p = paramsRef.current;

      const ctx = phos.begin();
      const s = scopeRef.current;
      if (s && s.signal && s.xy.length >= 4 && s !== drawn) {
        drawn = s;
        const W = cv.width;
        const cx = W / 2;
        const rate = s.rate > 0 ? s.rate : 48000;
        const kRef = Math.max(0.001, p.focus * (VEL_REF_RATE / rate));
        const buckets: Path2D[] = [];
        for (let b = 0; b < VEL_BUCKETS; b++) buckets.push(new Path2D());
        const xy = s.xy;
        let prevY = NaN;
        let prevLin = NaN;
        for (let i = 0; i + 1 < xy.length; i += 2) {
          const lin = (xy[i] + xy[i + 1]) / 2; // mono downmix, matching the backend's own convention
          const db = toDbLin(Math.abs(lin));
          const frac = Math.max(0, Math.min(1, (db - LEVEL_MIN_DB) / -LEVEL_MIN_DB));
          const y = (1 - frac) * cv.height; // canvas y=0 (top) = 0 dBFS, matching the bar's own scale
          if (!Number.isNaN(prevLin)) {
            const dist = Math.abs(lin - prevLin) * LIN_DIST_SCALE;
            const f = dist <= kRef ? 1 : Math.max(VEL_FLOOR, kRef / dist); // ~1/velocity, floored
            let b = (f * VEL_BUCKETS) | 0;
            if (b >= VEL_BUCKETS) b = VEL_BUCKETS - 1;
            buckets[b].moveTo(cx, prevY);
            buckets[b].lineTo(cx, y);
          }
          prevY = y;
          prevLin = lin;
        }
        ctx.globalCompositeOperation = "lighter";
        ctx.lineWidth = Math.max(1, W * p.beamWidth);
        // Flat (not round) caps: consecutive samples landing in different velocity buckets are
        // stroked separately, and round caps at their shared point would overlap and add into a
        // bright dot at every such sample — see Vectorscope's identical note.
        ctx.lineCap = "butt";
        // Beam blanking: brightness ∝ velocity factor, reaching zero at the fastest bucket (a beam
        // moving too fast to expose the phosphor draws nothing) — skip bucket 0.
        for (let b = 1; b < VEL_BUCKETS; b++) {
          ctx.strokeStyle = `rgba(${ar},${ag},${ab},${(p.glow * b) / (VEL_BUCKETS - 1)})`;
          ctx.stroke(buckets[b]);
        }
        ctx.globalCompositeOperation = "source-over";
      } else if (!s || !s.signal) {
        drawn = s; // idle: draw nothing this frame — the accumulator's own decay fades the trail out
      }

      phos.commit(dt, p.trailTau, p.tail);
      raf = requestAnimationFrame(render);
    };
    raf = requestAnimationFrame(render);
    return () => {
      cancelAnimationFrame(raf);
      phos.dispose(); // frees the GL textures/programs; a leaked context would survive the unmount
    };
  }, []);

  if (err) return <div className="meter meter-err">{t("meter.unavailable", { error: err })}</div>;

  const live = bar?.signal === true;
  const showNums = live && nums != null;

  const num = (v: number | undefined) => (showNums && v != null ? fmt(v) : "—");

  const set = <K extends keyof Params>(k: K, v: Params[K]) => setParams((prev) => ({ ...prev, [k]: v }));
  const CONTROLS: { key: keyof Params; label: string; min: number; max: number; step: number }[] = [
    { key: "trailTau", label: t("scope.trail"), min: 0.02, max: 0.6, step: 0.01 },
    { key: "tail", label: t("scope.tail"), min: 1, max: 24, step: 1 },
    { key: "glow", label: t("scope.glow"), min: 0.02, max: 1, step: 0.02 },
    { key: "focus", label: t("scope.focus"), min: 1, max: 24, step: 0.5 },
    { key: "beamWidth", label: t("scope.beam"), min: 0.1, max: 1, step: 0.05 },
  ];

  return (
    <div className={`meter${live ? "" : " meter-idle"}`}>
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
        <div className="vs-tuning meter-tuning">
          <div className="vs-tune-head">
            <span className="vs-tune-title">{t("scope.tune")}</span>
            <button type="button" className="vs-tune-reset" onClick={saveAsDefault}>
              {t("scope.saveDefault")}
            </button>
            <button type="button" className="vs-tune-reset" onClick={resetToFactory}>
              {t("scope.reset")}
            </button>
            <button type="button" className="vs-tune-close" title={t("scope.close")} aria-label={t("scope.close")} onClick={() => setTuning(false)}>
              ×
            </button>
          </div>
          {CONTROLS.map((cc) => {
            const dp = cc.step >= 1 ? 0 : cc.step >= 0.1 ? 1 : 2;
            return (
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
                <b>{params[cc.key].toFixed(dp)}</b>
              </label>
            );
          })}
        </div>
      )}
      <div
        className="meter-bars"
        style={
          plotBox
            ? { flex: "none", marginTop: `${plotBox.top + BAR_TOP_GAP}px`, height: `${Math.max(20, plotBox.height - BAR_TOP_GAP)}px` }
            : undefined
        }
      >
        <div className="vbar-group" title={t("meter.levelTitle")}>
          <div className="vbar" ref={levelBarRef} aria-hidden="true">
            <canvas ref={beamCanvasRef} className="vbar-beam" width={beamSize.w} height={beamSize.h} />
            <i className="vbar-ticks" />
            {live && <i className="vbar-rms" style={{ bottom: mark(pctOf(bar!.rms_db, LEVEL_MIN_DB)) }} />}
            {live && <i className="vbar-peak" style={{ bottom: mark(pctOf(bar!.peak_db, LEVEL_MIN_DB)) }} />}
          </div>
          <span className="vbar-cap">dBFS</span>
        </div>
        <div className="vbar-scale" aria-hidden="true">
          <div className="vbar-scale-track">
            {[0, -10, -20, -30, -40].map((db) => (
              <span key={db} style={{ bottom: `${pctOf(db, LEVEL_MIN_DB)}%` }}>
                {db}
              </span>
            ))}
          </div>
          <span className="vbar-cap">dB</span>
        </div>
        <div className="vbar-group" title={t("meter.lufsTitle")}>
          <div className="vbar" aria-hidden="true">
            <i className="vbar-lufs" style={{ height: live ? `${pctOf(bar!.short_term_lufs, LUFS_MIN)}%` : "0%" }} />
            <i className="vbar-ticks" />
            {live && <i className="vbar-lufs-m" style={{ bottom: mark(pctOf(bar!.momentary_lufs, LUFS_MIN)) }} />}
          </div>
          <span className="vbar-cap">LUFS</span>
        </div>
      </div>
      <div className="meter-read">
        <span className="mr-k">pk</span>
        <span className="mr-v">{num(nums?.peak_db)}</span>
        <span className="mr-k">M</span>
        <span className="mr-v">{num(nums?.momentary_lufs)}</span>
        <span className="mr-k">rms</span>
        <span className="mr-v">{num(nums?.rms_db)}</span>
        <span className="mr-k">S</span>
        <span className="mr-v">{num(nums?.short_term_lufs)}</span>
      </div>
    </div>
  );
}
