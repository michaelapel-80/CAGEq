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
  | { kind: "Isp"; db_over: number }
  | { kind: "Chirp"; f0: number; f1: number; duration_secs: number; log: boolean }
  | { kind: "Am"; carrier_hz: number; mod_hz: number; depth: number }
  | { kind: "Fm"; carrier_hz: number; mod_hz: number; deviation_hz: number };
type GeneratorRequest = {
  device: string | null;
  signal: Signal;
  level_dbfs: number;
  rate_override: number | null;
  seconds: number | null;
  unsafe_mode: boolean;
};
type AudioDevice = { id: string; name: string; eqapo_pattern: string; eqapo_enabled: boolean };

// The one flat selection the UI offers — mirrors testtone.rs's mutually-exclusive CLI flags
// (--sine/--square/--triangle/--sawtooth/--pulse/--chirp-log/--chirp-linear/--am/--fm/--pink/
// --white/--isp) as one dropdown instead of a signal-kind selector plus a separate waveform-shape
// sub-selector. "Chirp" covers both --chirp-log/--chirp-linear — log vs. linear is its own field
// below, the same way Tone's five shapes are flattened into SELECTIONS but a shape's own params
// (hz) live in a conditional block rather than the dropdown.
type Selected = Waveform | "Chirp" | "Am" | "Fm" | "Pink" | "White" | "Isp";
const TONE_WAVEFORMS: Waveform[] = ["Sine", "Square", "Triangle", "Sawtooth", "Pulse"];
const SELECTIONS: Selected[] = [...TONE_WAVEFORMS, "Chirp", "Am", "Fm", "Pink", "White", "Isp"];
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
  const [chirpF0, setChirpF0] = useState(20);
  const [chirpF1, setChirpF1] = useState(20000);
  const [chirpDuration, setChirpDuration] = useState(8);
  const [chirpLog, setChirpLog] = useState(true);
  const [amCarrier, setAmCarrier] = useState(1000);
  const [amMod, setAmMod] = useState(5);
  const [amDepth, setAmDepth] = useState(1);
  const [fmCarrier, setFmCarrier] = useState(1000);
  const [fmMod, setFmMod] = useState(5);
  const [fmDeviation, setFmDeviation] = useState(200);
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
        : selected === "Chirp"
          ? { kind: "Chirp", f0: chirpF0, f1: chirpF1, duration_secs: chirpDuration, log: chirpLog }
          : selected === "Am"
            ? { kind: "Am", carrier_hz: amCarrier, mod_hz: amMod, depth: amDepth }
            : selected === "Fm"
              ? { kind: "Fm", carrier_hz: fmCarrier, mod_hz: fmMod, deviation_hz: fmDeviation }
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

      {selected === "Chirp" && (
        <>
          <label className="row">
            {t("toneGen.chirpF0")}
            <ScrubNumber
              value={chirpF0}
              onInput={setChirpF0}
              onCommit={setChirpF0}
              min={1}
              max={40000}
              mode="mult"
              arrowStep={1.05}
              decimals={0}
              ariaLabel={t("toneGen.chirpF0")}
              style={{ width: "5em" }}
            />
            Hz
          </label>
          <label className="row">
            {t("toneGen.chirpF1")}
            <ScrubNumber
              value={chirpF1}
              onInput={setChirpF1}
              onCommit={setChirpF1}
              min={1}
              max={40000}
              mode="mult"
              arrowStep={1.05}
              decimals={0}
              ariaLabel={t("toneGen.chirpF1")}
              style={{ width: "5em" }}
            />
            Hz
          </label>
          <label className="row">
            {t("toneGen.chirpDuration")}
            <ScrubNumber
              value={chirpDuration}
              onInput={setChirpDuration}
              onCommit={setChirpDuration}
              min={0.5}
              max={60}
              mode="add"
              arrowStep={0.5}
              decimals={1}
              ariaLabel={t("toneGen.chirpDuration")}
              style={{ width: "4em" }}
            />
            s
          </label>
          <label className="row" style={{ alignItems: "center", gap: "0.4em" }}>
            <input type="checkbox" checked={chirpLog} onChange={(e) => setChirpLog(e.currentTarget.checked)} />
            {t("toneGen.chirpLog")}
          </label>
          <p style={{ fontSize: "0.75em", opacity: 0.7 }}>{t("toneGen.chirpLogHint")}</p>
        </>
      )}

      {selected === "Am" && (
        <>
          <label className="row">
            {t("toneGen.amCarrier")}
            <ScrubNumber
              value={amCarrier}
              onInput={setAmCarrier}
              onCommit={setAmCarrier}
              min={1}
              max={40000}
              mode="mult"
              arrowStep={1.05}
              decimals={0}
              ariaLabel={t("toneGen.amCarrier")}
              style={{ width: "5em" }}
            />
            Hz
          </label>
          <label className="row">
            {t("toneGen.amMod")}
            <ScrubNumber
              value={amMod}
              onInput={setAmMod}
              onCommit={setAmMod}
              min={0.1}
              max={20000}
              mode="mult"
              arrowStep={1.05}
              decimals={1}
              ariaLabel={t("toneGen.amMod")}
              style={{ width: "5em" }}
            />
            Hz
          </label>
          <label className="row">
            {t("toneGen.amDepth")}
            <ScrubNumber
              value={amDepth}
              onInput={setAmDepth}
              onCommit={setAmDepth}
              min={0}
              max={1}
              mode="add"
              arrowStep={0.05}
              decimals={2}
              ariaLabel={t("toneGen.amDepth")}
              style={{ width: "4em" }}
            />
          </label>
        </>
      )}

      {selected === "Fm" && (
        <>
          <label className="row">
            {t("toneGen.fmCarrier")}
            <ScrubNumber
              value={fmCarrier}
              onInput={setFmCarrier}
              onCommit={setFmCarrier}
              min={1}
              max={40000}
              mode="mult"
              arrowStep={1.05}
              decimals={0}
              ariaLabel={t("toneGen.fmCarrier")}
              style={{ width: "5em" }}
            />
            Hz
          </label>
          <label className="row">
            {t("toneGen.fmMod")}
            <ScrubNumber
              value={fmMod}
              onInput={setFmMod}
              onCommit={setFmMod}
              min={0.1}
              max={20000}
              mode="mult"
              arrowStep={1.05}
              decimals={1}
              ariaLabel={t("toneGen.fmMod")}
              style={{ width: "5em" }}
            />
            Hz
          </label>
          <label className="row">
            {t("toneGen.fmDeviation")}
            <ScrubNumber
              value={fmDeviation}
              onInput={setFmDeviation}
              onCommit={setFmDeviation}
              min={0}
              max={20000}
              mode="add"
              arrowStep={10}
              decimals={0}
              ariaLabel={t("toneGen.fmDeviation")}
              style={{ width: "5em" }}
            />
            Hz
          </label>
        </>
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
