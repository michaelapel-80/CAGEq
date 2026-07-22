import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Band } from "./biquad";
import { EqChart, Marker, RefCurve, Series } from "./EqChart";
import "./App.css";

type Headphone = { source: string; form_factor: string; name: string; path: string; rig: string };
type Target = { name: string; path: string };
type AudioDevice = { id: string; name: string; eqapo_pattern: string; eqapo_enabled: boolean };
type ApplyResult = {
  hash: string;
  device: string;
  cageq_path: string;
  cageq_text: string;
  preamp_db: number;
  clipping_warning: boolean;
  filters: Band[]; // bands written (AutoEq fit + custom) — drawn by the §5.2 chart
  reference_curve: { f: number; db: number }[]; // §5.2 ideal-correction overlay (empty for Dry)
};
type Status = {
  startup: string;
  health: string;
  health_kind: string; // "Running" | "Recovering" | "Terminal" | "-"
  recoveries: number;
  config_dir: string;
  config_source: string;
  sidecar: string;
};
type LoudnessMode = "Comparison" | "FinalVolume";
type LoudnessSettings = { base_pregain_db: number; mode: LoudnessMode };
type LoudnessUpdate = { settings: LoudnessSettings; applied: ApplyResult | null };
type SlotName = "A" | "B" | "Dry";
type FilterKind = "Peaking" | "LowShelf" | "HighShelf";
type CustomFilter = { kind: FilterKind; freq_hz: number; gain_db: number; q: number };
type SlotInputs = { model: string; measurementPath: string; targetPath: string; customFilters: CustomFilter[] };
type Selection = { headphone: string | null; target: string | null };
const FILTER_KINDS: FilterKind[] = ["Peaking", "LowShelf", "HighShelf"];

// §3.4 tone layer. The macro shelves use AutoEq's own constants (105 Hz / 10 kHz, Q 0.7)
// so a "bass boost" here means the same thing it does in AutoEq.
const MACRO_BASS = { kind: "LowShelf" as FilterKind, freq_hz: 105, q: 0.7 };
const MACRO_TREBLE = { kind: "HighShelf" as FilterKind, freq_hz: 10000, q: 0.7 };
const TONE_PRESETS: { name: string; filters: CustomFilter[] }[] = [
  { name: "Flat", filters: [] },
  { name: "Bass boost", filters: [{ ...MACRO_BASS, gain_db: 6 }] },
  { name: "Treble boost", filters: [{ ...MACRO_TREBLE, gain_db: 5 }] },
  { name: "V-shape", filters: [{ ...MACRO_BASS, gain_db: 5 }, { ...MACRO_TREBLE, gain_db: 4 }] },
  { name: "Warm", filters: [{ ...MACRO_BASS, gain_db: 4 }, { ...MACRO_TREBLE, gain_db: -3 }] },
  { name: "Bright", filters: [{ ...MACRO_BASS, gain_db: -2 }, { ...MACRO_TREBLE, gain_db: 4 }] },
];

// oratory1990 is the common reference measurement — default to it when a model has it.
const measurementRank = (h: Headphone) => (h.source === "oratory1990" ? 0 : 1);
// Slot A = goldenrod, Slot B = blue, Dry = neutral (filter.md §5.2 accent colours).
const SLOT_COLOR: Record<SlotName, string> = { A: "#daa520", B: "#3b82f6", Dry: "#9ca3af" };
const SLOT_ORDER: SlotName[] = ["A", "B", "Dry"]; // A-S-D keyboard order
const TONE_COLOR = "#16a34a"; // the editable tone layer
const REF_COLOR = "#a855f7"; // AutoEq's ideal-correction reference (target the fit chases)

function App() {
  const [status, setStatus] = useState<Status | null>(null);
  const [headphones, setHeadphones] = useState<Headphone[]>([]);
  const [targets, setTargets] = useState<Target[]>([]);
  const [devices, setDevices] = useState<AudioDevice[]>([]);
  const [deviceId, setDeviceId] = useState("");
  const [query, setQuery] = useState(""); // headphone-model search / selected model name
  const [measurementPath, setMeasurementPath] = useState(""); // chosen measurement (source) path
  const [targetPath, setTargetPath] = useState("");
  const [customFilters, setCustomFilters] = useState<CustomFilter[]>([]); // §3.4 manual filters
  const [result, setResult] = useState<ApplyResult | null>(null);
  const [loudness, setLoudness] = useState<LoudnessSettings | null>(null);
  const [confirmFinalVolume, setConfirmFinalVolume] = useState(true); // §7.5 point 1
  const [pendingFinal, setPendingFinal] = useState<{ next: LoudnessSettings; jump: number } | null>(null);
  const [dontAskAgain, setDontAskAgain] = useState(false);
  const [activeSlot, setActiveSlot] = useState<SlotName>("A");
  // Last-applied inputs per editable slot (for display + reloading the controls).
  const [slotInputs, setSlotInputs] = useState<Record<"A" | "B", SlotInputs | null>>({ A: null, B: null });
  // Last-applied composed bands per editable slot, so the §5.2 chart can overlay the
  // inactive slot's curve for a visual A/B alongside the active one (the active slot's
  // bands come from `result`; Dry is flat).
  const [slotCurves, setSlotCurves] = useState<Record<"A" | "B", Band[] | null>>({ A: null, B: null });
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(true);
  const [applying, setApplying] = useState(false);

  useEffect(() => {
    (async () => {
      try {
        setStatus(await invoke<Status>("status"));
        setLoudness(await invoke<LoudnessSettings>("get_loudness"));
        setConfirmFinalVolume(await invoke<boolean>("get_confirm_final_volume"));
        const [hp, tg, dev] = await Promise.all([
          invoke<{ headphones: Headphone[] }>("list_headphones"),
          invoke<{ targets: Target[] }>("list_targets"),
          invoke<AudioDevice[]>("list_devices"),
        ]);
        setHeadphones(hp.headphones);
        setTargets(tg.targets);
        setDevices(dev);
        // Prefer a device EqAPO is actually installed on, so the default selection works.
        const initial = dev.find((d) => d.eqapo_enabled) ?? dev[0];
        setDeviceId(initial?.id ?? "");
        if (initial) await invoke("set_device", { device: initial.eqapo_pattern });
        // Restore the last-used headphone/target (backfilled by path); fall back to the
        // Harman default target when nothing was saved.
        const sel = await invoke<Selection>("get_selection");
        const savedHp = sel.headphone ? hp.headphones.find((h) => h.path === sel.headphone) : undefined;
        if (savedHp) {
          setQuery(savedHp.name);
          setMeasurementPath(savedHp.path);
        }
        const savedTarget = sel.target && tg.targets.some((t) => t.path === sel.target) ? sel.target : undefined;
        const harman = tg.targets.find((t) => /harman over-ear 2018$/i.test(t.name));
        setTargetPath(savedTarget ?? harman?.path ?? tg.targets[0]?.path ?? "");
      } catch (e) {
        setError(String(e));
      } finally {
        setLoading(false);
      }
    })();
  }, []);

  // Poll status so the fail-safe banner reflects live watchdog health (trip/recover).
  useEffect(() => {
    const id = setInterval(() => {
      invoke<Status>("status").then(setStatus).catch(() => {});
    }, 3000);
    return () => clearInterval(id);
  }, []);

  async function retry() {
    try {
      setError("");
      await invoke("retry");
      setStatus(await invoke<Status>("status"));
    } catch (e) {
      setError(String(e));
    }
  }

  // Persist a loudness change; the backend re-applies the current config so the
  // shown preamp/config updates live (applied is null when nothing is applied yet).
  async function updateLoudness(next: LoudnessSettings) {
    setLoudness(next); // optimistic
    try {
      setError("");
      const update = await invoke<LoudnessUpdate>("set_loudness", { settings: next });
      setLoudness(update.settings); // clamped/authoritative value from the backend
      if (update.applied) setResult(update.applied);
    } catch (e) {
      setError(String(e));
    }
  }

  // Route mode/pre-gain changes; gate a switch to Final volume behind the §7.5 confirm
  // dialog, but only when it's actually a volume increase and the confirm is enabled.
  async function requestLoudness(next: LoudnessSettings) {
    const goingFinal = next.mode === "FinalVolume" && loudness?.mode !== "FinalVolume";
    if (goingFinal && confirmFinalVolume) {
      try {
        const jump = await invoke<number | null>("preview_loudness", { settings: next });
        if (jump != null && jump > 0.5) {
          setDontAskAgain(false);
          setPendingFinal({ next, jump });
          return;
        }
      } catch {
        /* preview failed — fall through to a direct (still ramped) apply */
      }
    }
    await updateLoudness(next);
  }

  async function confirmFinal() {
    const p = pendingFinal;
    setPendingFinal(null);
    if (!p) return;
    if (dontAskAgain) {
      setConfirmFinalVolume(false);
      await invoke("set_confirm_final_volume", { enabled: false });
    }
    await updateLoudness(p.next);
  }

  async function toggleConfirmFinalVolume(enabled: boolean) {
    setConfirmFinalVolume(enabled);
    await invoke("set_confirm_final_volume", { enabled });
  }

  // Group measurements by model name, so the same model measured by several sources
  // is one model with several measurements (not N merged rows). oratory1990 first.
  const byModel = useMemo(() => {
    const m = new Map<string, Headphone[]>();
    for (const h of headphones) {
      const arr = m.get(h.name);
      if (arr) arr.push(h);
      else m.set(h.name, [h]);
    }
    for (const arr of m.values())
      arr.sort((a, b) => measurementRank(a) - measurementRank(b) || a.source.localeCompare(b.source));
    return m;
  }, [headphones]);

  // Model names matching the search (capped so 2000+ models stay responsive).
  const modelMatches = useMemo(() => {
    if (query.length < 2) return [];
    const q = query.toLowerCase();
    return [...byModel.keys()].filter((n) => n.toLowerCase().includes(q)).slice(0, 200);
  }, [query, byModel]);

  // Measurements available for the currently-selected model (empty until one is picked).
  const measurements = byModel.get(query) ?? [];

  // Picking a model auto-selects its default (oratory1990-first) measurement.
  function onModelInput(value: string) {
    setQuery(value);
    const ms = byModel.get(value);
    setMeasurementPath(ms ? ms[0].path : "");
  }

  /** `auto` = triggered by a live tone edit: no button spinner, no error nagging. */
  async function apply(auto = false) {
    if (activeSlot === "Dry") return; // Dry is a fixed reference, not editable
    if (!measurementPath) {
      if (!auto) setError("Pick a headphone model and measurement first.");
      return;
    }
    const dev = devices.find((d) => d.id === deviceId);
    if (!dev) {
      if (!auto) setError("Pick an output device first.");
      return;
    }
    try {
      setError("");
      inFlight.current = true;
      if (!auto) setApplying(true);
      const applied = await invoke<ApplyResult>("apply", {
        device: dev.eqapo_pattern, // the EqAPO-matchable device pattern, not the headphone
        headphone: measurementPath,
        target: targetPath || null,
        slot: activeSlot,
        customFilters,
      });
      setResult(applied);
      setSlotInputs((prev) => ({
        ...prev,
        [activeSlot]: { model: query, measurementPath, targetPath, customFilters },
      }));
      setSlotCurves((prev) => ({ ...prev, [activeSlot]: applied.filters }));
      if (!auto) setStatus(await invoke<Status>("status"));
    } catch (e) {
      setError(String(e));
      if (!auto) setResult(null);
    } finally {
      inFlight.current = false;
      if (!auto) setApplying(false);
    }
  }

  // --- live tone editing -----------------------------------------------------
  // Tone changes auto-apply so they're audible immediately. A coalescing throttle
  // keeps a drag streaming updates (rather than only firing when you let go) without
  // flooding IPC; the Core separately enforces the §5.3 >=15 ms write spacing.
  const inFlight = useRef(false);
  const applyTimer = useRef<number | null>(null);
  const autoApplyRef = useRef<() => void>(() => {});

  // Refreshed every render so a queued timer never fires against stale state.
  autoApplyRef.current = () => {
    if (inFlight.current) {
      requestApply(60); // a write is in progress — retry shortly
      return;
    }
    void apply(true);
  };

  function requestApply(delay = 60) {
    if (applyTimer.current != null) return; // fold into the already-scheduled write
    applyTimer.current = window.setTimeout(() => {
      applyTimer.current = null;
      autoApplyRef.current();
    }, delay);
  }

  // Switch the active comparison slot. Populated A/B and Dry write instantly (cached,
  // no re-fit); switching to an empty A/B just makes it the editable target.
  async function switchSlot(slot: SlotName) {
    if (slot === activeSlot) return;
    setActiveSlot(slot);
    if (slot !== "Dry") {
      const s = slotInputs[slot];
      if (s) {
        setQuery(s.model);
        setMeasurementPath(s.measurementPath);
        setTargetPath(s.targetPath);
        setCustomFilters(s.customFilters);
      }
      if (!s) return; // empty slot: nothing written yet, user will configure + apply
    }
    try {
      setError("");
      const applied = await invoke<ApplyResult>("activate_slot", { slot });
      setResult(applied);
      if (slot !== "Dry") setSlotCurves((prev) => ({ ...prev, [slot]: applied.filters }));
    } catch (e) {
      setError(String(e));
    }
  }

  // Copy one editable slot onto the other and make it active — a starting point for
  // a variant (the target's cageq.txt is identical until you tweak it).
  async function copySlot(from: "A" | "B", to: "A" | "B") {
    const src = slotInputs[from];
    if (!src) {
      setError(`Slot ${from} is empty — apply something to it first.`);
      return;
    }
    try {
      setError("");
      const applied = await invoke<ApplyResult>("copy_slot", { from, to });
      setSlotInputs((prev) => ({ ...prev, [to]: src }));
      setSlotCurves((prev) => ({ ...prev, [to]: applied.filters }));
      setActiveSlot(to);
      setQuery(src.model);
      setMeasurementPath(src.measurementPath);
      setTargetPath(src.targetPath);
      setCustomFilters(src.customFilters);
      setResult(applied);
    } catch (e) {
      setError(String(e));
    }
  }

  async function changeDevice(id: string) {
    setDeviceId(id);
    const dev = devices.find((d) => d.id === id);
    if (dev) await invoke("set_device", { device: dev.eqapo_pattern });
  }

  // §3.4 macro shelves. Matching on kind AND frequency (not frequency alone) so a
  // hand-made filter that happens to sit near 105 Hz isn't hijacked by the slider.
  const macroGain = (spec: typeof MACRO_BASS) =>
    customFilters.find((f) => f.kind === spec.kind && Math.abs(f.freq_hz - spec.freq_hz) < 1)?.gain_db ?? 0;
  const setMacro = (spec: typeof MACRO_BASS, gain_db: number) => {
    setCustomFilters((cf) => {
      const i = cf.findIndex((f) => f.kind === spec.kind && Math.abs(f.freq_hz - spec.freq_hz) < 1);
      return i >= 0 ? cf.map((f, j) => (j === i ? { ...f, gain_db } : f)) : [...cf, { ...spec, gain_db }];
    });
    requestApply(60); // sliders stream, so throttle like a drag
  };

  // §3.4 tone editing — every change auto-applies (throttled).
  const addFilter = () => {
    setCustomFilters((cf) => [...cf, { kind: "Peaking", freq_hz: 1000, gain_db: 0, q: 1 }]);
    requestApply(0);
  };
  const updateFilter = (i: number, patch: Partial<CustomFilter>, delay = 0) => {
    setCustomFilters((cf) => cf.map((f, j) => (j === i ? { ...f, ...patch } : f)));
    requestApply(delay);
  };
  const removeFilter = (i: number) => {
    setCustomFilters((cf) => cf.filter((_, j) => j !== i));
    requestApply(0);
  };
  const setTonePreset = (filters: CustomFilter[]) => {
    setCustomFilters(filters.map((f) => ({ ...f })));
    requestApply(0);
  };

  // A/S/D switch slots, W toggles loudness mode — but not while typing in a field
  // (filter.md §5.2 blind-comparison shortcuts).
  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      const el = document.activeElement;
      if (el && /^(INPUT|SELECT|TEXTAREA)$/.test(el.tagName)) return;
      if (e.key === "a") switchSlot("A");
      else if (e.key === "s") switchSlot("B");
      else if (e.key === "d") switchSlot("Dry");
      else if (e.key === "w" && loudness)
        requestLoudness({ ...loudness, mode: loudness.mode === "Comparison" ? "FinalVolume" : "Comparison" });
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  const selectedDevice = devices.find((d) => d.id === deviceId);
  const dryActive = activeSlot === "Dry";

  // The active slot's composed bands split into its AutoEq fit and the tone offset. The
  // sidecar appends custom filters after the AutoEq bands, so the fit is everything
  // before the tone tail (§3.4). Drawn as fixed diamonds; the tone bands stay draggable.
  const autoEqBands = useMemo(() => {
    if (!result || dryActive) return [];
    const n = Math.max(0, result.filters.length - customFilters.length);
    return result.filters.slice(0, n);
  }, [result, customFilters, dryActive]);

  // §5.2 chart overlays: every populated slot's curve at once (inactive ones muted, the
  // active one prominent), plus the editable tone offset. Slot ids are stable across
  // active/inactive so a legend hide-toggle survives switching slots.
  const chartSeries: Series[] = useMemo(() => {
    if (!result) return [];
    const out: Series[] = [];
    for (const s of SLOT_ORDER) {
      if (s === activeSlot) continue;
      if (s === "Dry") out.push({ id: "slot-Dry", bands: [], color: SLOT_COLOR.Dry, label: "Dry (flat)", muted: true });
      else if (slotCurves[s]) out.push({ id: `slot-${s}`, bands: slotCurves[s]!, color: SLOT_COLOR[s], label: `Slot ${s}`, muted: true });
    }
    out.push({
      id: `slot-${activeSlot}`,
      bands: result.filters,
      color: SLOT_COLOR[activeSlot],
      label: dryActive ? "Dry (flat)" : `Slot ${activeSlot} total`,
    });
    if (!dryActive) out.push({ id: "tone", bands: customFilters, color: TONE_COLOR, label: "Tone (your offset)" });
    return out;
  }, [result, activeSlot, dryActive, slotCurves, customFilters]);

  const chartMarkers: Marker[] = useMemo(
    () => (autoEqBands.length ? [{ id: "autoeq", bands: autoEqBands, color: SLOT_COLOR[activeSlot], label: "AutoEq fit" }] : []),
    [autoEqBands, activeSlot],
  );

  // The ideal correction the active slot's fit chases (§5.2): the AutoEq curve should
  // hug it; the gap is the residual the parametric fit couldn't capture. Off for Dry.
  const chartRefs: RefCurve[] = useMemo(
    () =>
      !dryActive && result?.reference_curve?.length
        ? [{ id: "ideal", points: result.reference_curve, color: REF_COLOR, label: "Ideal correction (target)" }]
        : [],
    [result, dryActive],
  );

  // Fail-safe / startup banner (§5.1). Watchdog states are live; the startup verdicts
  // only matter until the user applies something (they describe the state at launch).
  const banner = (() => {
    if (!status) return null;
    if (status.health_kind === "Terminal")
      return {
        critical: true,
        text: "Safety shutdown: the audio DSP couldn't be recovered, so EQ is muted.",
        retry: true,
      };
    if (status.health_kind === "Recovering")
      return { critical: false, text: "Recovering the audio DSP… (safe state active, EQ muted)", retry: false };
    if (!result && status.startup === "SafeStateStillActive")
      return {
        critical: false,
        text: "A safety shutdown from a previous session is still active (EQ muted). Apply a correction to restore it.",
        retry: false,
      };
    if (!result && status.startup === "ExternallyModified")
      return {
        critical: false,
        text: "cageq.txt was changed outside CAGEq since the last run — the shown state may not match what's applied.",
        retry: false,
      };
    return null;
  })();

  return (
    <main className="app">
      {pendingFinal && (
        <div
          onClick={() => setPendingFinal(null)}
          style={{
            position: "fixed",
            inset: 0,
            background: "#0006",
            display: "flex",
            alignItems: "center",
            justifyContent: "center",
            zIndex: 10,
          }}
        >
          <div
            onClick={(e) => e.stopPropagation()}
            style={{ background: "var(--bg, #fff)", color: "inherit", border: "1px solid #0003", borderRadius: 8, padding: "1.2em", maxWidth: "26em", textAlign: "left" }}
          >
            <p style={{ marginTop: 0 }}>
              Switching to <b>Final volume</b> raises the volume by{" "}
              <b>+{pendingFinal.jump.toFixed(1)} dB</b> (ramped in at 6 dB/s). Continue?
            </p>
            <label style={{ fontSize: "0.85em", display: "block", margin: "0.6em 0" }}>
              <input type="checkbox" checked={dontAskAgain} onChange={(e) => setDontAskAgain(e.currentTarget.checked)} />{" "}
              Don't ask again (re-enable in the Loudness panel)
            </label>
            <div className="row" style={{ justifyContent: "flex-end", gap: "0.5em" }}>
              <button type="button" onClick={() => setPendingFinal(null)}>
                Cancel
              </button>
              <button type="button" onClick={confirmFinal}>
                Switch to Final volume
              </button>
            </div>
          </div>
        </div>
      )}

      {banner && (
        <div
          style={{
            border: `1px solid ${banner.critical ? "#c0392b" : "#b8860b"}`,
            background: banner.critical ? "#c0392b18" : "#b8860b14",
            color: banner.critical ? "#c0392b" : "#8a6d00",
            borderRadius: 6,
            padding: "0.6em 0.8em",
            margin: "0 0 0.8em",
            fontSize: "0.9em",
          }}
        >
          {banner.critical ? "⛔ " : "⚠ "}
          {banner.text}
          {banner.retry && (
            <button type="button" onClick={retry} style={{ marginLeft: "0.6em" }}>
              Retry
            </button>
          )}
        </div>
      )}

      {/* ---- header: the "set once per session" inputs (§5.1) ---- */}
      <header className="app-header">
        <h1>CAGEq</h1>
        {!loading && (
          <>
            <span className="row" style={{ gap: "0.4em" }}>
              <label htmlFor="device-select" style={{ fontSize: "0.85em", opacity: 0.75 }}>
                Output
              </label>
              {devices.length === 0 ? (
                <span style={{ opacity: 0.7, fontSize: "0.85em" }}>no active playback device</span>
              ) : (
                <select id="device-select" value={deviceId} onChange={(e) => changeDevice(e.currentTarget.value)}>
                  {devices.map((d) => (
                    <option key={d.id} value={d.id}>
                      {d.name}
                      {d.eqapo_enabled ? "" : " — ⚠ Equalizer APO not installed"}
                    </option>
                  ))}
                </select>
              )}
            </span>
            <form
              className="row"
              style={{ gap: "0.4em" }}
              onSubmit={(e) => {
                e.preventDefault();
                apply();
              }}
            >
              <label style={{ fontSize: "0.85em", opacity: 0.75 }}>Headphone</label>
              <input
                list="model-list"
                value={query}
                onChange={(e) => onModelInput(e.currentTarget.value)}
                placeholder={`Search ${byModel.size} models…`}
                style={{ minWidth: "14em" }}
                disabled={dryActive}
              />
              <datalist id="model-list">
                {modelMatches.map((name) => (
                  <option key={name} value={name} />
                ))}
              </datalist>
              <select
                value={measurementPath}
                onChange={(e) => setMeasurementPath(e.currentTarget.value)}
                disabled={dryActive || measurements.length === 0}
                title="Measurement source / rig"
              >
                {measurements.length === 0 ? (
                  <option value="">— pick a model —</option>
                ) : (
                  measurements.map((m) => (
                    <option key={m.path} value={m.path}>
                      by {m.source}
                      {m.rig ? ` on ${m.rig}` : ""}
                    </option>
                  ))
                )}
              </select>
            </form>
          </>
        )}
        {status && (
          <span style={{ fontSize: "0.72em", opacity: 0.6, marginLeft: "auto", textAlign: "right" }}>
            {status.sidecar} · {status.health}
            <br />
            {status.config_source}
          </span>
        )}
      </header>

      {!loading && selectedDevice && !selectedDevice.eqapo_enabled && (
        <p style={{ color: "#b8860b", fontSize: "0.85em", margin: "0 0 0.8em" }}>
          ⚠ Equalizer APO isn't installed on this device, so applying an EQ here has no effect. Enable it
          for this device with Equalizer APO's <em>Configurator</em> (DeviceSelector.exe), then reboot.
        </p>
      )}

      {loading && <p>Loading AutoEq catalogue…</p>}

      <div className="app-main">
        {/* ================= LEFT: target + macros + chart + bands ================= */}
        <section>
          {!loading && (
            <div className="panel">
              <h2>Correction</h2>
              <div className="row" style={{ gap: "0.5em" }}>
                <label style={{ fontSize: "0.85em", opacity: 0.75 }}>Target</label>
                <select value={targetPath} onChange={(e) => setTargetPath(e.currentTarget.value)} disabled={dryActive}>
                  {targets.map((t) => (
                    <option key={t.path} value={t.path}>
                      {t.name}
                    </option>
                  ))}
                </select>
                <button type="button" onClick={() => apply()} disabled={applying || dryActive} style={{ marginLeft: "auto" }}>
                  {applying ? "Fitting…" : dryActive ? "Dry (pick A or B)" : `Apply → Slot ${activeSlot}`}
                </button>
              </div>

              {/* macro shelves sit directly above the diagram (§5.1) */}
              <div style={{ marginTop: "0.7em", opacity: dryActive ? 0.5 : 1 }}>
                {(
                  [
                    ["Bass", MACRO_BASS],
                    ["Treble", MACRO_TREBLE],
                  ] as const
                ).map(([label, spec]) => (
                  <div key={label} className="row" style={{ gap: "0.5em", marginBottom: "0.25em" }}>
                    <span style={{ fontSize: "0.8em", width: "3.5em" }}>{label}</span>
                    <input
                      type="range"
                      min={-12}
                      max={12}
                      step={0.5}
                      value={macroGain(spec)}
                      disabled={dryActive}
                      onChange={(e) => setMacro(spec, Number(e.currentTarget.value))}
                      style={{ flex: 1, maxWidth: "18em" }}
                    />
                    <span
                      style={{ fontSize: "0.8em", width: "4em", textAlign: "right", fontVariantNumeric: "tabular-nums" }}
                    >
                      {macroGain(spec) > 0 ? "+" : ""}
                      {macroGain(spec).toFixed(1)} dB
                    </span>
                  </div>
                ))}
              </div>

              {result && (
                <>
                  <EqChart
                    series={chartSeries}
                    markers={chartMarkers}
                    refs={chartRefs}
                    nodes={{
                      bands: customFilters,
                      color: TONE_COLOR,
                      disabled: dryActive,
                      onChange: (i, patch) => updateFilter(i, patch, 70),
                      onDragEnd: () => requestApply(0),
                    }}
                  />
                  <p style={{ fontSize: "0.8em", opacity: 0.75, margin: "0.2em 0 0" }}>
                    Preamp <code>{result.preamp_db.toFixed(1)} dB</code>{" "}
                    {loudness?.mode === "FinalVolume" ? "(max clipping-free)" : "(Auto-LUFS)"} · hash{" "}
                    <code>{result.hash}</code>
                  </p>
                  {result.clipping_warning && (
                    <p style={{ color: "#b8860b", fontSize: "0.8em", margin: "0.2em 0 0" }}>
                      ⚠ Emergency clipping protection active instead of the loudness match — extreme peak.
                    </p>
                  )}
                </>
              )}
            </div>
          )}

          {/* ---- tone bands (the detailed editor, §5.2 filter list) ---- */}
          {!loading && (
            <div className="panel" style={{ opacity: dryActive ? 0.5 : 1 }}>
              <h2>Tone bands (§3.4)</h2>
              {customFilters.length === 0 && (
                <p style={{ fontSize: "0.8em", opacity: 0.7, margin: "0 0 0.5em" }}>
                  No tone filters — the AutoEq correction is used as-is.
                </p>
              )}
              {customFilters.map((f, i) => (
                <div key={i} className="row" style={{ gap: "0.4em", marginBottom: "0.3em" }}>
                  <select
                    value={f.kind}
                    disabled={dryActive}
                    onChange={(e) => updateFilter(i, { kind: e.currentTarget.value as FilterKind })}
                  >
                    {FILTER_KINDS.map((k) => (
                      <option key={k} value={k}>
                        {k}
                      </option>
                    ))}
                  </select>
                  <label style={{ fontSize: "0.8em" }}>
                    Fc{" "}
                    <input
                      type="number" min={20} max={20000} step={10} value={f.freq_hz} disabled={dryActive}
                      onChange={(e) => updateFilter(i, { freq_hz: Number(e.currentTarget.value) })}
                      style={{ width: "5.5em" }}
                    />{" "}
                    Hz
                  </label>
                  <label style={{ fontSize: "0.8em" }}>
                    Gain{" "}
                    <input
                      type="number" min={-20} max={20} step={0.5} value={f.gain_db} disabled={dryActive}
                      onChange={(e) => updateFilter(i, { gain_db: Number(e.currentTarget.value) })}
                      style={{ width: "4em" }}
                    />{" "}
                    dB
                  </label>
                  <label style={{ fontSize: "0.8em" }}>
                    Q{" "}
                    <input
                      type="number" min={0.1} max={20} step={0.1} value={f.q} disabled={dryActive}
                      onChange={(e) => updateFilter(i, { q: Number(e.currentTarget.value) })}
                      style={{ width: "4em" }}
                    />
                  </label>
                  <button type="button" onClick={() => removeFilter(i)} disabled={dryActive} title="Remove">
                    ✕
                  </button>
                </div>
              ))}
              <button type="button" onClick={addFilter} disabled={dryActive} style={{ fontSize: "0.85em" }}>
                + Add filter
              </button>
              {customFilters.length > 0 && (
                <span style={{ fontSize: "0.75em", opacity: 0.6, marginLeft: "0.6em" }}>
                  takes effect on Apply → Slot {activeSlot}
                </span>
              )}
            </div>
          )}
        </section>

        {/* ================= RIGHT: slots + loudness + presets ================= */}
        <aside>
          {!loading && (
            <div className="panel">
              <h2>Compare</h2>
              <div className="row" style={{ gap: "0.4em" }}>
                {SLOT_ORDER.map((s) => {
                  const active = s === activeSlot;
                  const populated = s === "Dry" || slotInputs[s] !== null;
                  const key = s === "A" ? "A" : s === "B" ? "S" : "D";
                  return (
                    <button
                      key={s}
                      type="button"
                      onClick={() => switchSlot(s)}
                      title={`${s} (key ${key})${populated ? "" : " — empty"}`}
                      style={{
                        borderWidth: 2,
                        borderStyle: "solid",
                        borderColor: active ? SLOT_COLOR[s] : "transparent",
                        color: active ? SLOT_COLOR[s] : undefined,
                        fontWeight: active ? 700 : 400,
                        opacity: populated || active ? 1 : 0.55,
                      }}
                    >
                      {s === "Dry" ? "Dry" : `Slot ${s}`} <kbd style={{ fontSize: "0.7em", opacity: 0.6 }}>{key}</kbd>
                    </button>
                  );
                })}
              </div>
              <div className="row" style={{ gap: "0.4em", marginTop: "0.4em" }}>
                <span style={{ fontSize: "0.75em", opacity: 0.6 }}>Copy:</span>
                <button type="button" onClick={() => copySlot("A", "B")} disabled={!slotInputs.A} style={{ fontSize: "0.8em" }}>
                  A→B
                </button>
                <button type="button" onClick={() => copySlot("B", "A")} disabled={!slotInputs.B} style={{ fontSize: "0.8em" }}>
                  B→A
                </button>
              </div>
              <p style={{ fontSize: "0.75em", opacity: 0.6, margin: "0.5em 0 0" }}>
                A / S / D switch slots, W toggles the loudness mode — even without looking at the screen.
              </p>
              {loudness?.mode === "FinalVolume" && (slotInputs.A !== null || slotInputs.B !== null) && (
                <p style={{ color: "#b8860b", fontSize: "0.78em", margin: "0.4em 0 0" }}>
                  ⚠ Final volume: each slot plays at its own max volume, so A/B/Dry aren't loudness-matched
                  — a louder slot can just sound "better". Press <kbd>W</kbd> for Comparison to A/B fairly.
                </p>
              )}
            </div>
          )}

          {loudness && (
            <div className="panel">
              <h2>Loudness (§4.0)</h2>
              <label style={{ display: "block", marginBottom: "0.2em" }}>
                <input
                  type="radio"
                  name="loudness-mode"
                  checked={loudness.mode === "Comparison"}
                  onChange={() => requestLoudness({ ...loudness, mode: "Comparison" })}
                />{" "}
                Comparison (A/B-fair)
              </label>
              <label style={{ display: "block", marginBottom: "0.4em" }}>
                <input
                  type="radio"
                  name="loudness-mode"
                  checked={loudness.mode === "FinalVolume"}
                  onChange={() => requestLoudness({ ...loudness, mode: "FinalVolume" })}
                />{" "}
                Final volume (loudest safe)
              </label>
              <label className="row" style={{ opacity: loudness.mode === "Comparison" ? 1 : 0.4, fontSize: "0.85em" }}>
                Base pre-gain
                <input
                  type="number"
                  min={-40}
                  max={0}
                  step={1}
                  value={loudness.base_pregain_db}
                  disabled={loudness.mode !== "Comparison"}
                  onChange={(e) => updateLoudness({ ...loudness, base_pregain_db: Number(e.currentTarget.value) })}
                  style={{ width: "4.5em" }}
                />
                dB
              </label>
              <p style={{ fontSize: "0.75em", opacity: 0.7, margin: "0.5em 0 0" }}>
                {loudness.mode === "Comparison"
                  ? "Every curve ends up equally loud, so A/B comparisons judge timbre, not level."
                  : "Maximum clipping-free volume — base pre-gain and loudness match are disabled."}
              </p>
              <label style={{ fontSize: "0.75em", opacity: 0.8, display: "block", marginTop: "0.5em" }}>
                <input
                  type="checkbox"
                  checked={confirmFinalVolume}
                  onChange={(e) => toggleConfirmFinalVolume(e.currentTarget.checked)}
                />{" "}
                Confirm before switching to Final volume
              </label>
            </div>
          )}

          {!loading && (
            <div className="panel" style={{ opacity: dryActive ? 0.5 : 1 }}>
              <h2>Tone presets</h2>
              <p style={{ fontSize: "0.75em", opacity: 0.7, margin: "0 0 0.6em" }}>
                Your tonal preference on top of the correction — starts flat, everything here is an offset.
              </p>
              <div className="row" style={{ gap: "0.35em" }}>
                {TONE_PRESETS.map((p) => (
                  <button
                    key={p.name}
                    type="button"
                    disabled={dryActive}
                    onClick={() => setTonePreset(p.filters)}
                    style={{ fontSize: "0.8em" }}
                  >
                    {p.name}
                  </button>
                ))}
              </div>
            </div>
          )}
        </aside>
      </div>

      {error && <p style={{ color: "crimson" }}>{error}</p>}

      {result && (
        <details style={{ marginTop: "0.5em", fontSize: "0.8em" }}>
          <summary style={{ cursor: "pointer", opacity: 0.7 }}>Written config — {result.cageq_path}</summary>
          <pre style={{ textAlign: "left", background: "#0002", padding: "0.75em", overflowX: "auto" }}>
            {result.cageq_text}
          </pre>
        </details>
      )}
    </main>
  );
}

export default App;
