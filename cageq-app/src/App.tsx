import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import "./App.css";

type Headphone = { source: string; form_factor: string; name: string; path: string };
type Target = { name: string; path: string };
type AudioDevice = { id: string; name: string; eqapo_pattern: string; eqapo_enabled: boolean };
type ApplyResult = {
  hash: string;
  device: string;
  cageq_path: string;
  cageq_text: string;
  preamp_db: number;
  clipping_warning: boolean;
};
type Status = {
  startup: string;
  health: string;
  recoveries: number;
  config_dir: string;
  config_source: string;
  sidecar: string;
};
type LoudnessMode = "Comparison" | "FinalVolume";
type LoudnessSettings = { base_pregain_db: number; mode: LoudnessMode };
type LoudnessUpdate = { settings: LoudnessSettings; applied: ApplyResult | null };
type SlotName = "A" | "B" | "Dry";
type SlotInputs = { query: string; targetPath: string };
type Selection = { headphone: string | null; target: string | null };

const display = (h: Headphone) => `${h.name} · ${h.source} · ${h.form_factor}`;
// Slot A = goldenrod, Slot B = blue, Dry = neutral (filter.md §5.2 accent colours).
const SLOT_COLOR: Record<SlotName, string> = { A: "#daa520", B: "#3b82f6", Dry: "#9ca3af" };
const SLOT_ORDER: SlotName[] = ["A", "B", "Dry"]; // A-S-D keyboard order

function App() {
  const [status, setStatus] = useState<Status | null>(null);
  const [headphones, setHeadphones] = useState<Headphone[]>([]);
  const [targets, setTargets] = useState<Target[]>([]);
  const [devices, setDevices] = useState<AudioDevice[]>([]);
  const [deviceId, setDeviceId] = useState("");
  const [query, setQuery] = useState("");
  const [targetPath, setTargetPath] = useState("");
  const [result, setResult] = useState<ApplyResult | null>(null);
  const [loudness, setLoudness] = useState<LoudnessSettings | null>(null);
  const [activeSlot, setActiveSlot] = useState<SlotName>("A");
  // Last-applied inputs per editable slot (for display + reloading the controls).
  const [slotInputs, setSlotInputs] = useState<Record<"A" | "B", SlotInputs | null>>({ A: null, B: null });
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(true);
  const [applying, setApplying] = useState(false);

  useEffect(() => {
    (async () => {
      try {
        setStatus(await invoke<Status>("status"));
        setLoudness(await invoke<LoudnessSettings>("get_loudness"));
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
        if (savedHp) setQuery(display(savedHp));
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

  // Filter client-side and cap the datalist so 6800+ entries stay responsive.
  const matches = useMemo(() => {
    if (query.length < 2) return [];
    const q = query.toLowerCase();
    return headphones.filter((h) => display(h).toLowerCase().includes(q)).slice(0, 200);
  }, [query, headphones]);

  async function apply() {
    if (activeSlot === "Dry") return; // Dry is a fixed reference, not editable
    const hp = headphones.find((h) => display(h) === query);
    if (!hp) {
      setError("Pick a headphone from the list first.");
      return;
    }
    const dev = devices.find((d) => d.id === deviceId);
    if (!dev) {
      setError("Pick an output device first.");
      return;
    }
    try {
      setError("");
      setApplying(true);
      setResult(
        await invoke<ApplyResult>("apply", {
          device: dev.eqapo_pattern, // the EqAPO-matchable device pattern, not the headphone
          headphone: hp.path,
          target: targetPath || null,
          slot: activeSlot,
        })
      );
      setSlotInputs((prev) => ({ ...prev, [activeSlot]: { query, targetPath } }));
      setStatus(await invoke<Status>("status"));
    } catch (e) {
      setError(String(e));
      setResult(null);
    } finally {
      setApplying(false);
    }
  }

  // Switch the active comparison slot. Populated A/B and Dry write instantly (cached,
  // no re-fit); switching to an empty A/B just makes it the editable target.
  async function switchSlot(slot: SlotName) {
    if (slot === activeSlot) return;
    setActiveSlot(slot);
    if (slot !== "Dry") {
      const s = slotInputs[slot];
      if (s) {
        setQuery(s.query);
        setTargetPath(s.targetPath);
      }
      if (!s) return; // empty slot: nothing written yet, user will configure + apply
    }
    try {
      setError("");
      setResult(await invoke<ApplyResult>("activate_slot", { slot }));
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
      setActiveSlot(to);
      setQuery(src.query);
      setTargetPath(src.targetPath);
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
        updateLoudness({ ...loudness, mode: loudness.mode === "Comparison" ? "FinalVolume" : "Comparison" });
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  const selectedDevice = devices.find((d) => d.id === deviceId);
  const dryActive = activeSlot === "Dry";

  return (
    <main className="container">
      <h1>CAGEq</h1>
      <p>Caged Auto-Gain EQ — AutoEq database</p>

      {status && (
        <p style={{ fontSize: "0.8em", opacity: 0.75 }}>
          sidecar: {status.sidecar} · health: {status.health}
          <br />
          config: {status.config_source}
          <br />
          <span style={{ opacity: 0.7 }}>writes to: {status.config_dir}</span>
        </p>
      )}

      {!loading && (
        <div>
          <p className="row" style={{ alignItems: "center", gap: "0.5em" }}>
            <label htmlFor="device-select">Output device:</label>
            {devices.length === 0 ? (
              <span style={{ opacity: 0.7 }}>no active playback device detected</span>
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
          </p>
          {selectedDevice && !selectedDevice.eqapo_enabled && (
            <p style={{ color: "#b8860b", fontSize: "0.85em", margin: "0 0 0.5em" }}>
              ⚠ Equalizer APO isn't installed on this device, so applying an EQ here has no effect.
              Enable it for this device with Equalizer APO's <em>Configurator</em> (DeviceSelector.exe),
              then reboot.
            </p>
          )}
        </div>
      )}

      {!loading && (
        <div style={{ margin: "0.75em 0" }}>
          <div className="row" style={{ gap: "0.4em", alignItems: "center" }}>
            <span style={{ fontSize: "0.85em", opacity: 0.75 }}>Compare:</span>
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
            <span style={{ marginLeft: "0.6em", fontSize: "0.75em", opacity: 0.6 }}>Copy:</span>
            <button type="button" onClick={() => copySlot("A", "B")} disabled={!slotInputs.A} style={{ fontSize: "0.8em" }}>
              A→B
            </button>
            <button type="button" onClick={() => copySlot("B", "A")} disabled={!slotInputs.B} style={{ fontSize: "0.8em" }}>
              B→A
            </button>
          </div>
          <p style={{ fontSize: "0.75em", opacity: 0.6, margin: "0.3em 0 0" }}>
            A / S / D switch slots, W toggles the loudness mode — even without looking at the screen.
          </p>
          {loudness?.mode === "FinalVolume" && (slotInputs.A !== null || slotInputs.B !== null) && (
            <p style={{ color: "#b8860b", fontSize: "0.78em", margin: "0.4em 0 0" }}>
              ⚠ Final volume: each slot plays at its own max volume, so A/B/Dry aren't loudness-matched —
              a louder slot can just sound "better". Press <kbd>W</kbd> for Comparison to A/B fairly.
            </p>
          )}
        </div>
      )}

      {loading ? (
        <p>Loading AutoEq catalogue…</p>
      ) : (
        <form
          className="row"
          onSubmit={(e) => {
            e.preventDefault();
            apply();
          }}
        >
          <input
            list="hp-list"
            value={query}
            onChange={(e) => setQuery(e.currentTarget.value)}
            placeholder={`Search ${headphones.length} headphones…`}
            style={{ minWidth: "22em" }}
            disabled={dryActive}
          />
          <datalist id="hp-list">
            {matches.map((h) => (
              <option key={h.path} value={display(h)} />
            ))}
          </datalist>
          <select value={targetPath} onChange={(e) => setTargetPath(e.currentTarget.value)} disabled={dryActive}>
            {targets.map((t) => (
              <option key={t.path} value={t.path}>
                {t.name}
              </option>
            ))}
          </select>
          <button type="submit" disabled={applying || dryActive}>
            {applying ? "Fitting…" : dryActive ? "Dry (pick A or B to edit)" : `Apply → Slot ${activeSlot}`}
          </button>
        </form>
      )}

      {loudness && (
        <fieldset style={{ marginTop: "1em", textAlign: "left", border: "1px solid #0003", borderRadius: 6 }}>
          <legend>Loudness (§4.0)</legend>
          <div className="row" style={{ alignItems: "center", flexWrap: "wrap", gap: "0.75em" }}>
            <label>
              <input
                type="radio"
                name="loudness-mode"
                checked={loudness.mode === "Comparison"}
                onChange={() => updateLoudness({ ...loudness, mode: "Comparison" })}
              />{" "}
              Comparison (A/B-fair)
            </label>
            <label>
              <input
                type="radio"
                name="loudness-mode"
                checked={loudness.mode === "FinalVolume"}
                onChange={() => updateLoudness({ ...loudness, mode: "FinalVolume" })}
              />{" "}
              Final volume (loudest safe)
            </label>
            <label style={{ opacity: loudness.mode === "Comparison" ? 1 : 0.4 }}>
              Base pre-gain:{" "}
              <input
                type="number"
                min={-40}
                max={0}
                step={1}
                value={loudness.base_pregain_db}
                disabled={loudness.mode !== "Comparison"}
                onChange={(e) =>
                  updateLoudness({ ...loudness, base_pregain_db: Number(e.currentTarget.value) })
                }
                style={{ width: "5em" }}
              />{" "}
              dB
            </label>
          </div>
          <p style={{ fontSize: "0.78em", opacity: 0.7, margin: "0.5em 0 0" }}>
            {loudness.mode === "Comparison"
              ? "Every curve ends up equally loud (base pre-gain + loudness match) so A/B comparisons judge timbre, not level."
              : "Maximum clipping-free volume (peak at 0 dBFS) — base pre-gain and loudness match are disabled."}
          </p>
        </fieldset>
      )}

      {error && <p style={{ color: "crimson" }}>{error}</p>}

      {result && (
        <>
          <p>
            Preamp <code>{result.preamp_db.toFixed(1)} dB</code>{" "}
            {loudness?.mode === "FinalVolume" ? "(max clipping-free)" : "(Auto-LUFS loudness match)"} · hash{" "}
            <code>{result.hash}</code> → {result.cageq_path}
          </p>
          {result.clipping_warning && (
            <p style={{ color: "#b8860b" }}>
              ⚠ Emergency clipping protection active instead of the loudness match — this curve has an
              extreme peak.
            </p>
          )}
          <pre
            style={{
              textAlign: "left",
              background: "#0002",
              padding: "0.75em",
              overflowX: "auto",
            }}
          >
            {result.cageq_text}
          </pre>
        </>
      )}
    </main>
  );
}

export default App;
