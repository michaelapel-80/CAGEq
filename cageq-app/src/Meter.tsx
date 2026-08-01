import { useEffect, useState } from "react";
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
};

// Bar scale: dBFS mapped across the meter width. -60..0 covers the useful range.
const BAR_MIN_DB = -60;
const barPct = (db: number) => Math.max(0, Math.min(100, ((db - BAR_MIN_DB) / -BAR_MIN_DB) * 100));
const fmt = (v: number) => (v > 0 ? "+" : "") + v.toFixed(1);

export function Meter({ deviceId }: { deviceId: string }) {
  const [m, setM] = useState<MeterUpdate | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | undefined;
    (async () => {
      unlisten = await listen<MeterUpdate>("monitor", (e) => {
        if (active) setM(e.payload);
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

  const live = m?.signal === true;
  const peak = live ? m!.peak_db : BAR_MIN_DB;
  const rms = live ? m!.rms_db : BAR_MIN_DB;

  return (
    <div className={`meter${live ? "" : " meter-idle"}`}>
      <div className="meter-lufs" title="BS.1770 loudness — momentary (400 ms) / short-term (3 s)">
        <span className="meter-lufs-m">{live ? fmt(m!.momentary_lufs) : "—"}</span>
        <span className="meter-lufs-unit">LUFS·M</span>
        <span className="meter-lufs-s">S {live ? fmt(m!.short_term_lufs) : "—"}</span>
      </div>
      <div className="meter-bar" aria-hidden="true">
        <i className="meter-bar-rms" style={{ width: `${barPct(rms)}%` }} />
        <i className="meter-bar-peak" style={{ left: `${barPct(peak)}%` }} />
      </div>
      <div className="meter-nums">
        {live ? (
          <>
            peak <b>{fmt(m!.peak_db)}</b> · rms <b>{fmt(m!.rms_db)}</b> dBFS
          </>
        ) : (
          <span className="meter-nosignal">no signal — play audio to meter the output</span>
        )}
      </div>
    </div>
  );
}
