import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke } from "@tauri-apps/api/core";
import { ScrubNumber } from "./ScrubNumber";

// Mirrors `cageq_monitor::signal::Signal`'s `#[serde(tag = "kind")]` JSON shape exactly — see
// that module's own doc for why it's tagged this way. Kept local (not exported/shared with
// App.tsx) the same way `ScopeWindow.tsx` carries none of the main window's state.
type Waveform = "Sine" | "Square" | "Triangle" | "Sawtooth" | "Pulse";
type Signal =
  | { kind: "Tone"; waveform: Waveform; hz: number }
  | { kind: "Pink" }
  | { kind: "White" }
  | { kind: "Isp"; db_over: number };
type GeneratorRequest = {
  device: string | null;
  signal: Signal;
  level_dbfs: number;
  rate_override: number | null;
  seconds: number | null;
  unsafe_mode: boolean;
};
type AudioDevice = { id: string; name: string; eqapo_pattern: string; eqapo_enabled: boolean };

// The one flat selection the UI offers — mirrors testtone.rs's eight mutually-exclusive CLI flags
// (--sine/--square/--triangle/--sawtooth/--pulse/--pink/--white/--isp) as one dropdown instead of
// a signal-kind selector plus a separate waveform-shape sub-selector.
type Selected = Waveform | "Pink" | "White" | "Isp";
const TONE_WAVEFORMS: Waveform[] = ["Sine", "Square", "Triangle", "Sawtooth", "Pulse"];
const SELECTIONS: Selected[] = [...TONE_WAVEFORMS, "Pink", "White", "Isp"];
function isToneWaveform(s: Selected): s is Waveform {
  return (TONE_WAVEFORMS as Selected[]).includes(s);
}

// dB the true (reconstructed) peak sits above an Isp sample stream's own peak — mirrors
// `cageq_monitor::signal::ISP_MAX_OVER_DB` (kept in sync by hand; it's a fixed textbook constant,
// not something either side expects to change).
const ISP_MAX_OVER_DB = 3.0103;

/**
 * The detached test-tone generator window (loaded via `index.html#tone`, see main.tsx) — an
 * in-app front end for the dev `testtone` CLI (`cageq-monitor/examples/testtone.rs`), driving the
 * same signal synthesis (`cageq_monitor::signal`) through the `start_test_generator`/
 * `stop_test_signal` commands. Carries no state from the main window; every mount starts from safe
 * defaults (Pink, -20 dBFS, Unsafe off) — nothing here persists across a close/reopen, on purpose.
 */
export function ToneWindow() {
  const { t } = useTranslation();

  const [devices, setDevices] = useState<AudioDevice[]>([]);
  const [deviceId, setDeviceId] = useState<string>("");
  const [selected, setSelected] = useState<Selected>("Pink");
  const [hz, setHz] = useState(1000);
  const [dbOver, setDbOver] = useState(ISP_MAX_OVER_DB);
  const [levelDbfs, setLevelDbfs] = useState(-20);
  const [unsafeMode, setUnsafeMode] = useState(false);
  const [rateOverride, setRateOverride] = useState<string>("");
  const [seconds, setSeconds] = useState<string>("");
  const [playing, setPlaying] = useState(false);
  const [error, setError] = useState("");

  useEffect(() => {
    invoke<AudioDevice[]>("list_devices")
      .then((ds) => {
        setDevices(ds);
        if (ds.length > 0) setDeviceId((cur) => cur || ds[0].id);
      })
      .catch((e) => setError(String(e)));
    // Belt-and-suspenders: the backend also force-stops on window-destroy (it can't rely on this
    // running — see src-tauri's `on_window_event` doc) but this catches a plain in-app close too.
    return () => {
      void invoke("stop_test_signal").catch(() => {});
    };
  }, []);

  // Isp has no "safe" setting at all (file header doc, `testtone.rs`) — falling back out of
  // Unsafe while it's selected would otherwise leave a disabled-but-still-selected option, which
  // reads as broken rather than as the deliberate gate it is.
  useEffect(() => {
    if (!unsafeMode && selected === "Isp") setSelected("Pink");
  }, [unsafeMode, selected]);

  // -18 dBFS mirrors `cageq_monitor::signal::SAFE_PLAYBACK_CEILING_DBFS` (kept in sync by hand,
  // like ISP_MAX_OVER_DB above) — deliberately more conservative than testtone.rs's own -3 dBFS
  // ceiling, since that CLI needs a terminal to reach but this window is a GUI default anyone can
  // click into. The backend re-clamps to the same value regardless of what this sends.
  const levelCeiling = unsafeMode ? 0 : -18;
  // Unchecking Unsafe lowers the ceiling; re-clamp a level that's now above it rather than
  // leaving a stale, out-of-range value sitting in the field.
  useEffect(() => {
    setLevelDbfs((v) => Math.min(v, levelCeiling));
  }, [levelCeiling]);

  async function play() {
    setError("");
    const signal: Signal = isToneWaveform(selected)
      ? { kind: "Tone", waveform: selected, hz }
      : selected === "Isp"
        ? { kind: "Isp", db_over: dbOver }
        : { kind: selected };
    const req: GeneratorRequest = {
      device: deviceId || null,
      signal,
      level_dbfs: levelDbfs,
      rate_override: rateOverride.trim() ? Number(rateOverride) : null,
      seconds: seconds.trim() ? Number(seconds) : null,
      unsafe_mode: unsafeMode,
    };
    try {
      await invoke("start_test_generator", { req });
      setPlaying(true);
    } catch (e) {
      setError(String(e));
    }
  }

  async function stop() {
    setPlaying(false);
    try {
      await invoke("stop_test_signal");
    } catch (e) {
      setError(String(e));
    }
  }

  return (
    <div className="tone-window panel" style={{ maxWidth: 420, margin: "0 auto" }}>
      <h2 style={{ marginTop: 0 }}>{t("toneGen.title")}</h2>
      <p style={{ fontSize: "0.8em", opacity: 0.7 }}>{t("toneGen.intro")}</p>

      <label className="row">
        {t("toneGen.signal")}
        <select value={selected} onChange={(e) => setSelected(e.currentTarget.value as Selected)}>
          {SELECTIONS.map((s) => (
            <option key={s} value={s} disabled={s === "Isp" && !unsafeMode}>
              {t(`toneGen.waveform.${s}`)}
            </option>
          ))}
        </select>
      </label>

      {isToneWaveform(selected) && (
        <label className="row">
          {t("toneGen.frequency")}
          <ScrubNumber
            value={hz}
            onInput={setHz}
            onCommit={setHz}
            min={1}
            max={40000}
            mode="mult"
            arrowStep={1.05}
            decimals={0}
            ariaLabel={t("toneGen.frequency")}
            style={{ width: "5em" }}
          />
          Hz
        </label>
      )}

      {selected === "Isp" ? (
        <>
          <label className="row">
            {t("toneGen.dbOver")}
            <ScrubNumber
              value={dbOver}
              onInput={setDbOver}
              onCommit={setDbOver}
              min={0}
              max={ISP_MAX_OVER_DB}
              mode="add"
              arrowStep={0.1}
              decimals={4}
              ariaLabel={t("toneGen.dbOver")}
              style={{ width: "5em" }}
            />
            dBTP
          </label>
          <p style={{ fontSize: "0.75em", opacity: 0.7 }}>{t("toneGen.ispHint", { max: ISP_MAX_OVER_DB })}</p>
        </>
      ) : (
        <label className="row">
          {t("toneGen.level")}
          <ScrubNumber
            value={levelDbfs}
            onInput={setLevelDbfs}
            onCommit={setLevelDbfs}
            min={-80}
            max={levelCeiling}
            mode="add"
            arrowStep={1}
            decimals={1}
            ariaLabel={t("toneGen.level")}
            style={{ width: "4em" }}
          />
          dBFS ({t("toneGen.ceiling", { db: levelCeiling })})
        </label>
      )}

      <label className="row">
        {t("toneGen.device")}
        <select value={deviceId} onChange={(e) => setDeviceId(e.currentTarget.value)}>
          {devices.map((d) => (
            <option key={d.id} value={d.id}>
              {d.name}
            </option>
          ))}
        </select>
      </label>

      <details style={{ margin: "0.5em 0" }}>
        <summary style={{ cursor: "pointer", fontSize: "0.85em" }}>{t("toneGen.advanced")}</summary>
        <label className="row">
          {t("toneGen.rateOverride")}
          <input
            type="text"
            inputMode="decimal"
            placeholder={t("toneGen.rateOverridePlaceholder")}
            value={rateOverride}
            onChange={(e) => setRateOverride(e.currentTarget.value)}
          />
          Hz
        </label>
        <label className="row">
          {t("toneGen.seconds")}
          <input
            type="text"
            inputMode="decimal"
            placeholder={t("toneGen.secondsPlaceholder")}
            value={seconds}
            onChange={(e) => setSeconds(e.currentTarget.value)}
          />
        </label>
      </details>

      <label className="row" style={{ alignItems: "center", gap: "0.4em" }}>
        <input type="checkbox" checked={unsafeMode} onChange={(e) => setUnsafeMode(e.currentTarget.checked)} />
        {t("toneGen.unsafe")}
      </label>
      {unsafeMode && <p style={{ color: "#b8860b", fontSize: "0.8em" }}>{t("toneGen.unsafeWarning")}</p>}

      <div className="row" style={{ marginTop: "0.6em" }}>
        {playing ? (
          <button type="button" onClick={stop}>
            {t("toneGen.stop")}
          </button>
        ) : (
          <button type="button" onClick={play} disabled={devices.length === 0}>
            {t("toneGen.play")}
          </button>
        )}
      </div>

      {error && (
        <p style={{ color: "#c0392b", fontSize: "0.8em" }} role="alert">
          {error}
        </p>
      )}
    </div>
  );
}
