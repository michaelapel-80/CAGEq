import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke } from "@tauri-apps/api/core";
import { meterStream, type MeterData } from "./streams";

/**
 * §5.3c post-EQ output meter, laid out vertically to sit beside the chart (no vertical growth, so
 * it can be on by default). Subscribes to the backend's `monitor` events (WASAPI loopback on the
 * selected render endpoint — already post-EQ) and shows two bars:
 *   • Level (dBFS): the phosphor-histogram fill + peak (amber) and true-RMS (neutral) marks.
 *   • LUFS: BS.1770 momentary fill + short-term mark — makes the auto-LUFS preamp match visible.
 * Independent of the custom-APO work. Mounting starts capture, unmounting stops it.
 */

// Payload shape (`MeterData`, mirroring cageq-monitor's MeterUpdate) lives in streams.ts, which
// owns the channel-based transport — see there for why channels, not `listen` events.

// Level bar scale (dBFS). Must match the backend histogram range (BAR_MIN_DB) so the peak/RMS
// marks line up with the phosphor fill.
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

  if (err) return <div className="meter meter-err">{t("meter.unavailable", { error: err })}</div>;

  const live = bar?.signal === true;
  const bins = live && Array.isArray(bar!.bins) ? bar!.bins : [];
  const showNums = live && nums != null;

  // Vertical phosphor gradient (bottom = quietest segment). Per-segment alpha is the segment's
  // persistence brightness.
  const fillStyle =
    bins.length > 1
      ? {
          background: `linear-gradient(to top, ${bins
            .map((b, i) => {
              const alpha = Math.round(Math.max(0, Math.min(1, b)) * 100);
              const pos = ((i / (bins.length - 1)) * 100).toFixed(1);
              return `color-mix(in srgb, var(--accent) ${alpha}%, transparent) ${pos}%`;
            })
            .join(", ")})`,
        }
      : undefined;

  const num = (v: number | undefined) => (showNums && v != null ? fmt(v) : "—");

  return (
    <div className={`meter${live ? "" : " meter-idle"}`}>
      <div
        className="meter-bars"
        style={
          plotBox
            ? { flex: "none", marginTop: `${plotBox.top + BAR_TOP_GAP}px`, height: `${Math.max(20, plotBox.height - BAR_TOP_GAP)}px` }
            : undefined
        }
      >
        <div className="vbar-group" title={t("meter.levelTitle")}>
          <div className="vbar" aria-hidden="true">
            <i className="vbar-fill" style={fillStyle} />
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
