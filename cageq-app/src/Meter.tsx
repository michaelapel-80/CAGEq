import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/**
 * §5.3c post-EQ output meter. Subscribes to the backend's `monitor` events (WASAPI loopback on
 * the selected render endpoint — already post-EQ, so it shows what actually reaches the DAC) and
 * renders peak/RMS + BS.1770 momentary/short-term LUFS. The LUFS numbers are the point: they make
 * the auto-LUFS preamp match visible ("did it land?"). Independent of the custom-APO work.
 *
 * Mounting starts capture (`start_monitor`), unmounting stops it (`stop_monitor`); a device change
 * restarts on the new endpoint. Needs audio actually playing — an idle endpoint reads no signal.
 */

type MeterUpdate = {
  peak_db: number;
  rms_db: number;
  momentary_lufs: number;
  short_term_lufs: number;
  signal: boolean;
  /** Phosphor-persistence histogram: per-segment brightness 0..1, quietest segment first. */
  bins: number[];
};

// Bar scale: dBFS mapped across the meter width. -60..0 covers the useful range.
const BAR_MIN_DB = -60;
const barPct = (db: number) => Math.max(0, Math.min(100, ((db - BAR_MIN_DB) / -BAR_MIN_DB) * 100));
const fmt = (v: number) => (v > 0 ? "+" : "") + v.toFixed(1);
// The bar/line update every event (~60 fps) for smoothness; the numeric readouts refresh at this
// slower interval so peak/LUFS digits are readable instead of a 60 fps blur.
const NUMS_INTERVAL_MS = 200;

export function Meter({ deviceId }: { deviceId: string }) {
  const [bar, setBar] = useState<MeterUpdate | null>(null); // fast: phosphor bar + RMS line
  const [nums, setNums] = useState<MeterUpdate | null>(null); // throttled: peak/RMS/LUFS readouts
  const [err, setErr] = useState<string | null>(null);
  const lastNums = useRef(0);

  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | undefined;
    (async () => {
      unlisten = await listen<MeterUpdate>("monitor", (e) => {
        if (!active) return;
        setBar(e.payload);
        const now = performance.now();
        if (now - lastNums.current >= NUMS_INTERVAL_MS) {
          lastNums.current = now;
          setNums(e.payload);
        }
      });
      try {
        await invoke("start_monitor", { device: deviceId || null });
        setErr(null);
      } catch (e) {
        setErr(String(e));
      }
    })();
    return () => {
      active = false;
      unlisten?.();
      // Best-effort stop; a failure here just leaves the capture thread to be replaced next start.
      invoke("stop_monitor").catch(() => {});
    };
  }, [deviceId]);

  if (err) return <div className="meter meter-err">Meter unavailable: {err}</div>;

  const live = bar?.signal === true;
  const rms = live ? bar!.rms_db : BAR_MIN_DB;
  const peak = live ? bar!.peak_db : BAR_MIN_DB;
  const bins = live && Array.isArray(bar!.bins) ? bar!.bins : [];
  // Numeric readouts come from the throttled snapshot, and only once there is one and audio is live.
  const showNums = live && nums != null;
  // The bar is a horizontal gradient whose per-segment alpha is that segment's phosphor
  // brightness (bright where the level dwells, fading afterglow where peaks reached). The line
  // marks the true RMS on top.
  const fillStyle =
    bins.length > 1
      ? {
          background: `linear-gradient(to right, ${bins
            .map((b, i) => {
              const pct = Math.round(Math.max(0, Math.min(1, b)) * 100);
              const pos = ((i / (bins.length - 1)) * 100).toFixed(1);
              return `color-mix(in srgb, var(--accent) ${pct}%, transparent) ${pos}%`;
            })
            .join(", ")})`,
        }
      : undefined;

  return (
    <div className={`meter${live ? "" : " meter-idle"}`}>
      <div className="meter-lufs" title="BS.1770 loudness — momentary (400 ms) / short-term (3 s)">
        <span className="meter-lufs-m">{showNums ? fmt(nums!.momentary_lufs) : "—"}</span>
        <span className="meter-lufs-unit">LUFS·M</span>
        <span className="meter-lufs-s">S {showNums ? fmt(nums!.short_term_lufs) : "—"}</span>
      </div>
      <div className="meter-bar" aria-hidden="true">
        <i className="meter-bar-fill" style={fillStyle} />
        {live && <i className="meter-bar-rms" style={{ left: `clamp(0px, ${barPct(rms)}%, calc(100% - 2px))` }} />}
        {live && <i className="meter-bar-peak" style={{ left: `clamp(0px, ${barPct(peak)}%, calc(100% - 2px))` }} />}
      </div>
      <div className="meter-nums">
        {live ? (
          <>
            peak <b>{showNums ? fmt(nums!.peak_db) : "—"}</b> · rms <b>{showNums ? fmt(nums!.rms_db) : "—"}</b> dBFS
          </>
        ) : (
          <span className="meter-nosignal">no signal — play audio to meter the output</span>
        )}
      </div>
    </div>
  );
}
