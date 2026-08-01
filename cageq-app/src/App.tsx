import { useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import { Band } from "./biquad";
import { EqChart, Marker, PhaseCurve, RefCurve, Series } from "./EqChart";
import { ImpulseChart } from "./NerdCharts";
import { ToneGrid } from "./ToneGrid";
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
  g_target_db: number; // §4.1 loudness target the curve wants
  g_max_peak_db: number; // §4.2 composed-curve peak
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
type CustomFilter = { kind: FilterKind; freq_hz: number; gain_db: number; q: number; fixed?: boolean; enabled?: boolean; macro?: string };
// §3.4 custom EQ is split into three per-slot **stages** — organizational groups of bands
// that all sum into the one biquad cascade (EqAPO flattens everything; the Rust core still
// receives a single combined list, so it's unchanged). Each stage toggles on/off as a whole.
// The fixed Bass/Treble/Air macros live in `tone`.
type StageId = "fit" | "content" | "tone";
type Stage = { enabled: boolean; bands: CustomFilter[] };
type Stages = { fit: Stage; content: Stage; tone: Stage };
type SlotInputs = { model: string; measurementPath: string; targetPath: string; stages: Stages };
type Selection = { headphone: string | null; target: string | null };
// A slot's last computed fit, persisted so launch can write cageq.txt immediately without
// waiting on the ~1–2 s cold AutoEq fit (§3.5 launch-from-cache). Exactly the fields the
// core needs to seed a slot and compose the preamp: the composed bands + the two curve
// quantities (+ the chart reference). The fit is deterministic in its inputs, so the
// cached bands equal a fresh fit's; a background re-fit reconciles/​warms the sidecar.
type PersistedFit = { device: string; filters: Band[]; g_target_db: number; g_max_peak_db: number; reference_curve: { f: number; db: number }[] };
// §3.5 resume blob (UI-owned shape; the backend stores/returns it verbatim).
type Resume = {
  activeSlot: SlotName;
  deviceId: string;
  slots: { A: SlotInputs | null; B: SlotInputs | null };
  fits?: { A: PersistedFit | null; B: PersistedFit | null };
  activeStage?: StageId; // which stage tab was last selected (defaults to fit on first run)
};

// §3.4 tone layer — three always-present **fixed** grid bands (0 dB by default,
// non-removable, type locked), the classic warmth/brightness/air triad. They replace the
// old separate macro sliders; everything else is a normal, removable band.
//   • Bass   — LowShelf 105 Hz  (AutoEq's own constant): warmth / body.
//   • Treble — HighShelf 4 kHz: brightness / presence. Deliberately NOT AutoEq's 10 kHz
//     ("air") shelf — 10 kHz is barely a loudness event; 4 kHz sits at the K-weighting
//     peak, so it's the impactful control and the §4.1 match responds to it.
//   • Air    — HighShelf 12 kHz: the subtle top-octave sparkle the 4 kHz Treble gives up.
//     Splitting it off lets Treble stay impactful without reaching into the harsh
//     presence region, and gives "air" back as its own taste control.
// Each carries a frontend-only `macro` id so the grid can label the two high-shelves
// apart (Treble vs Air) and a restart-restore can re-seed the right band by identity.
const macroBass = (gain_db = 0): CustomFilter => ({ kind: "LowShelf", freq_hz: 105, gain_db, q: 0.7, fixed: true, macro: "Bass" });
const macroTreble = (gain_db = 0): CustomFilter => ({ kind: "HighShelf", freq_hz: 4000, gain_db, q: 0.7, fixed: true, macro: "Treble" });
const macroAir = (gain_db = 0): CustomFilter => ({ kind: "HighShelf", freq_hz: 12000, gain_db, q: 0.7, fixed: true, macro: "Air" });
/** A fresh tone layer: the three 0 dB macro bands (level-neutral). */
const defaultTone = (): CustomFilter[] => [macroBass(), macroTreble(), macroAir()];

/** Guarantee the three fixed macros are present (0 dB if absent), preserving any existing
 *  gain/Fc/Q/bypass — by `macro` id, or by shape for pre-Air saved slots (no id). Keeps
 *  non-fixed bands. Applied when loading tone from persisted/slot data. */
function ensureMacros(cf: CustomFilter[]): CustomFilter[] {
  const find = (macro: string, kind: FilterKind, freq: number) =>
    cf.find((f) => f.fixed && (f.macro === macro || (!f.macro && f.kind === kind && Math.abs(f.freq_hz - freq) < 1000)));
  const seed = (fresh: CustomFilter, found?: CustomFilter): CustomFilter =>
    found ? { ...fresh, gain_db: found.gain_db, freq_hz: found.freq_hz, q: found.q, enabled: found.enabled } : fresh;
  return [
    seed(macroBass(), find("Bass", "LowShelf", 105)),
    seed(macroTreble(), find("Treble", "HighShelf", 4000)),
    seed(macroAir(), find("Air", "HighShelf", 12000)),
    ...cf.filter((f) => !f.fixed),
  ];
}

// --- §3.4 stages -----------------------------------------------------------
// Three fixed, well-named stages (deliberately not user-definable — arbitrary stages lose
// the semantic grouping that's their whole point, and EqAPO sums them anyway). Order is
// display-only; the summed response is order-independent.
const STAGE_ORDER: StageId[] = ["fit", "content", "tone"];
const STAGE_META: Record<StageId, { label: string; hint: string }> = {
  fit: { label: "Fit", hint: "Personal correction on top of the target — pads, seal, your own ears." },
  content: { label: "Content", hint: "Adjustments for what you're playing — dialogue lift, a bright master." },
  tone: { label: "Tone", hint: "Your permanent taste. The fixed Bass / Treble / Air macros live here." },
};

/** A fresh three-stage set: Fit/Content empty, Tone seeded with the 0 dB fixed macros. */
const defaultStages = (): Stages => ({
  fit: { enabled: true, bands: [] },
  content: { enabled: true, bands: [] },
  tone: { enabled: true, bands: defaultTone() },
});

/** Coerce a persisted/partial stage set to the full shape: all three present, Tone's fixed
 *  macros guaranteed. Used on every load from slot / preset / resume data. */
function normalizeStages(raw?: Partial<Stages> | null): Stages {
  const stage = (s: Partial<Stage> | undefined, isTone: boolean): Stage => ({
    enabled: s?.enabled ?? true,
    bands: isTone ? ensureMacros(s?.bands ?? defaultTone()) : (s?.bands ?? []),
  });
  return { fit: stage(raw?.fit, false), content: stage(raw?.content, false), tone: stage(raw?.tone, true) };
}

/** The custom bands actually written: every enabled stage's enabled bands, in stage order —
 *  what rides to the sidecar (appended to the AutoEq fit) and drives the §4.1/§4.2 policy. */
const appliedBands = (st: Stages): CustomFilter[] =>
  STAGE_ORDER.flatMap((id) => (st[id].enabled ? st[id].bands.filter((b) => b.enabled !== false) : []));

/** Migrate a slot's inputs to the stages shape — new blobs already carry `stages`; a
 *  pre-stages blob had a single `customFilters` list, which becomes the Tone stage. */
function migrateInputs(inp: SlotInputs & { customFilters?: CustomFilter[] }): SlotInputs {
  const stages = inp.stages
    ? normalizeStages(inp.stages)
    : normalizeStages({ tone: { enabled: true, bands: inp.customFilters ?? defaultTone() } });
  return { model: inp.model, measurementPath: inp.measurementPath, targetPath: inp.targetPath, stages };
}

/** Migrate the saved library to the stages shape: pre-stages templates carried
 *  `customFilters` (→ a Tone-stage template), presets carried `customFilters` (→ all
 *  stages with just Tone filled). New-shape entries pass through normalized. */
type LegacyTemplate = { id: string; name: string; stage?: StageId; bands?: CustomFilter[]; customFilters?: CustomFilter[] };
type LegacyPreset = {
  id: string; name: string; model: string; measurementPath: string; targetPath: string;
  stages?: Partial<Stages>; customFilters?: CustomFilter[];
};
function normalizeLibrary(lib: { templates?: LegacyTemplate[]; presets?: LegacyPreset[] } | null): Library {
  const templates: FilterTemplate[] = (lib?.templates ?? []).map((t) =>
    t.stage
      ? { id: t.id, name: t.name, stage: t.stage, bands: t.bands ?? [] }
      : { id: t.id, name: t.name, stage: "tone", bands: ensureMacros(t.customFilters ?? []) },
  );
  const presets: UserPreset[] = (lib?.presets ?? []).map((p) => ({
    id: p.id,
    name: p.name,
    model: p.model,
    measurementPath: p.measurementPath,
    targetPath: p.targetPath,
    stages: p.stages ? normalizeStages(p.stages) : normalizeStages({ tone: { enabled: true, bands: p.customFilters ?? defaultTone() } }),
  }));
  return { presets, templates };
}

// Presets are bass/treble/air gain triples applied to the three fixed macro bands;
// picking one resets the tone to exactly those three bands at the given gains.
const TONE_PRESETS: { name: string; bass: number; treble: number; air: number }[] = [
  { name: "Flat", bass: 0, treble: 0, air: 0 },
  { name: "Bass boost", bass: 6, treble: 0, air: 0 },
  { name: "Treble boost", bass: 0, treble: 5, air: 0 },
  { name: "Airy", bass: 0, treble: 0, air: 5 },
  { name: "V-shape", bass: 5, treble: 4, air: 2 },
  { name: "Warm", bass: 4, treble: -3, air: -3 },
  { name: "Bright", bass: -2, treble: 4, air: 3 },
];

// §3.5 preset library. Two kinds, deliberately different in scope (filter.md §5.2):
//   • FilterTemplate — one **stage's** bands; loading drops them into that stage only.
//   • UserPreset     — a full setup (measurement + target + all stages); loading replaces all.
// Both persist in settings.json's opaque `library` blob (get_library/set_library), the
// same UI-owned-blob treatment as the resume state — no per-field Rust change.
type FilterTemplate = { id: string; name: string; stage: StageId; bands: CustomFilter[] };
type UserPreset = { id: string; name: string; model: string; measurementPath: string; targetPath: string; stages: Stages };
type Library = { presets: UserPreset[]; templates: FilterTemplate[] };

// The curated (built-in, read-only) filter templates: the classic tone shapes as the three
// fixed macro bands, all tagged to the Tone stage. A full session preset is inherently
// user-specific (it names a headphone), so there are no curated ones.
const CURATED_TEMPLATES: FilterTemplate[] = TONE_PRESETS.map((p) => ({
  id: `curated:${p.name}`,
  name: p.name,
  stage: "tone",
  bands: [macroBass(p.bass), macroTreble(p.treble), macroAir(p.air)],
}));

// Replace the entry sharing `entry.id`, or append it if new — so editing a saved item
// stays in place (no duplicate) while a fresh save lands at the end.
function upsert<T extends { id: string }>(list: T[], entry: T): T[] {
  return list.some((e) => e.id === entry.id) ? list.map((e) => (e.id === entry.id ? entry : e)) : [...list, entry];
}

// Centre frequency of the widest empty gap between existing bands, in *log* space (matching
// the chart's log axis) — with the 20 Hz–20 kHz edges as virtual neighbours so a lone band
// still gets a sensible slot. Used by the Add button so a new band never lands on a node.
function widestGapHz(bands: { freq_hz: number }[]): number {
  if (bands.length === 0) return 1000;
  const pts = [Math.log10(20), ...bands.map((b) => Math.log10(b.freq_hz)), Math.log10(20000)].sort((a, b) => a - b);
  let best = -1;
  let mid = Math.log10(1000);
  for (let i = 1; i < pts.length; i++) {
    const gap = pts[i] - pts[i - 1];
    if (gap > best) {
      best = gap;
      mid = (pts[i] + pts[i - 1]) / 2;
    }
  }
  return Math.round(10 ** mid);
}

// The IEC power glyph (line through an open arc) for the per-stage enable toggle — the app
// has no icon font, so it's inline SVG; inherits colour via currentColor.
const PowerGlyph = () => (
  <svg viewBox="0 0 16 16" width="17" height="17" aria-hidden="true">
    <path d="M8 3 L8 8.6" fill="none" stroke="currentColor" strokeWidth="1.9" strokeLinecap="round" />
    <path d="M5.1 5.3 A4.4 4.4 0 1 0 10.9 5.3" fill="none" stroke="currentColor" strokeWidth="1.9" strokeLinecap="round" />
  </svg>
);

// oratory1990 is the common reference measurement — default to it when a model has it.
const measurementRank = (h: Headphone) => (h.source === "oratory1990" ? 0 : 1);
// Slot A = goldenrod, Slot B = blue, Dry = neutral (filter.md §5.2 accent colours).
const SLOT_COLOR: Record<SlotName, string> = { A: "#daa520", B: "#3b82f6", Dry: "#9ca3af" };
const SLOT_ORDER: SlotName[] = ["A", "B", "Dry"]; // A-S-D keyboard order
// Distinct from the slot colours (A gold, B blue, Dry grey) and the ref purple: Fit cyan,
// Content pink, Tone green. (Content was amber — too close to Slot A's goldenrod.)
const STAGE_COLOR: Record<StageId, string> = { fit: "#0ea5e9", content: "#ec4899", tone: "#16a34a" };
const REF_COLOR = "#a855f7"; // AutoEq's ideal-correction reference (target the fit chases)
const RAW_COLOR = "#94a3b8"; // the raw headphone measurement (nerd overlay)
const TARGET_COLOR = "#7dd3fc"; // the target curve — pale blue, à la AutoEq (nerd overlay)
const PHASE_COLOR = "#f59e0b"; // the filter chain's phase, on the secondary axis (nerd overlay)

function App() {
  const [status, setStatus] = useState<Status | null>(null);
  const [headphones, setHeadphones] = useState<Headphone[]>([]);
  const [targets, setTargets] = useState<Target[]>([]);
  const [devices, setDevices] = useState<AudioDevice[]>([]);
  const [deviceId, setDeviceId] = useState("");
  const [query, setQuery] = useState(""); // headphone-model search / selected model name
  const [measurementPath, setMeasurementPath] = useState(""); // chosen measurement (source) path
  const [targetPath, setTargetPath] = useState("");
  const [stages, setStages] = useState<Stages>(defaultStages()); // §3.4 the three per-slot filter stages
  const [activeStage, setActiveStage] = useState<StageId>("fit"); // which stage the grid/chart edits (restored from resume)
  // A just-added band, born highlighted so it's never hunted for. The chart always pulses its
  // node; `focusGrid` additionally scrolls the grid column in and enters its Fc edit mode —
  // set only for the keyboard/Add path, so a mouse double-click on the chart isn't yanked off
  // the chart into the grid. `nonce` re-fires the effects even when the storage idx repeats.
  const [newBand, setNewBand] = useState<{ stage: StageId; idx: number; nonce: number; focusGrid: boolean } | null>(null);
  const newBandNonce = useRef(0);
  // Cross-view hover link: the storage index of the band the pointer is over in *either* the
  // chart or the grid, so the other view highlights the matching node/column (null = none).
  const [hoverBand, setHoverBand] = useState<number | null>(null);
  const [showAllStages, setShowAllStages] = useState(false); // library filter: templates of all stages vs the active one
  const [impulseView, setImpulseView] = useState(false); // §5.2: swap the chart for the impulse response
  const [rawCurve, setRawCurve] = useState<{ f: number; db: number }[] | null>(null); // raw measured FR (nerd overlay)
  const [targetCurve, setTargetCurve] = useState<{ f: number; db: number }[] | null>(null); // the target curve (nerd overlay)
  const [result, setResult] = useState<ApplyResult | null>(null);
  const [loudness, setLoudness] = useState<LoudnessSettings | null>(null);
  const [confirmFinalVolume, setConfirmFinalVolume] = useState(true); // §7.5 point 1
  const [pendingFinal, setPendingFinal] = useState<{ next: LoudnessSettings; jump: number } | null>(null);
  const [dontAskAgain, setDontAskAgain] = useState(false);
  // §3.5 preset library + the inline save form and a generic confirm dialog (reused for
  // the "overwrite existing?" prompt, mirroring the destructive-slot-action confirm).
  const [library, setLibrary] = useState<Library>({ presets: [], templates: [] });
  const [saveForm, setSaveForm] = useState<{ kind: "preset" | "template"; name: string; error: boolean } | null>(null);
  const [confirmBox, setConfirmBox] = useState<{ message: string; confirmLabel: string; onConfirm: () => void } | null>(null);
  const [activeSlot, setActiveSlot] = useState<SlotName>("A");
  // Drop a stale cross-view highlight when the underlying band list changes out from under a
  // still pointer (stage tab switch, slot change) — no pointerleave fires in that case.
  useEffect(() => setHoverBand(null), [activeStage, activeSlot]);
  // Last-applied inputs per editable slot (for display + reloading the controls).
  const [slotInputs, setSlotInputs] = useState<Record<"A" | "B", SlotInputs | null>>({ A: null, B: null });
  // Last computed fit per editable slot, persisted into the resume blob so the next launch
  // writes EQ immediately from cache instead of waiting on the cold sidecar fit (§3.5).
  const [slotFits, setSlotFits] = useState<Record<"A" | "B", PersistedFit | null>>({ A: null, B: null });
  // Which editable slots have a fit cached in the backend *this session*. After a
  // restart-restore the frontend has each slot's inputs but the backend cache is empty,
  // so switching to a not-yet-hydrated slot must re-fit rather than a pure re-write.
  const [hydrated, setHydrated] = useState<Record<"A" | "B", boolean>>({ A: false, B: false });
  // Gate resume-persistence until the initial restore has run, so we don't overwrite the
  // saved session with fresh defaults on the very first render.
  const [restored, setRestored] = useState(false);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(true);
  const [applying, setApplying] = useState(false);

  // Fit `inp` into `slot` via the sidecar and record it (cached bands, hydrated flag).
  // The single place a slot is (re)fitted; callers set `result` from the return value.
  async function writeFit(slot: "A" | "B", inp: SlotInputs, dev: AudioDevice): Promise<ApplyResult> {
    const applied = await invoke<ApplyResult>("apply", {
      device: dev.eqapo_pattern, // the EqAPO-matchable device pattern, not the headphone
      headphone: inp.measurementPath,
      target: inp.targetPath || null,
      slot,
      // Every enabled stage's enabled bands, summed into one list (§3.4). Bypassed bands and
      // disabled stages stay in the UI but are excluded from what's written.
      customFilters: appliedBands(inp.stages),
    });
    setSlotInputs((prev) => ({ ...prev, [slot]: inp }));
    setHydrated((prev) => ({ ...prev, [slot]: true }));
    // Remember the composed fit so the next launch can restore this slot from cache (§3.5).
    setSlotFits((prev) => ({
      ...prev,
      [slot]: {
        device: applied.device,
        filters: applied.filters,
        g_target_db: applied.g_target_db,
        g_max_peak_db: applied.g_max_peak_db,
        reference_curve: applied.reference_curve,
      },
    }));
    return applied;
  }

  useEffect(() => {
    (async () => {
      try {
        setStatus(await invoke<Status>("status"));
        setLoudness(await invoke<LoudnessSettings>("get_loudness"));
        setConfirmFinalVolume(await invoke<boolean>("get_confirm_final_volume"));
        const lib = await invoke<Parameters<typeof normalizeLibrary>[0]>("get_library");
        if (lib) setLibrary(normalizeLibrary(lib));
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

        // §3.5 resume: restore the last session and re-apply the active slot, so the app
        // matches reality on launch without a manual "Apply". Falls back to the older
        // selection-only restore when there's no resume blob (clean install / first run
        // after the feature landed).
        const resume = await invoke<Resume | null>("get_resume");
        const rdev = resume?.deviceId ? dev.find((d) => d.id === resume.deviceId) : undefined;
        const useDev = rdev ?? initial;
        setDeviceId(useDev?.id ?? "");
        if (useDev) await invoke("set_device", { device: useDev.eqapo_pattern });

        const activeSlotName = resume?.activeSlot ?? "A";
        const activeInp = resume && resume.activeSlot !== "Dry" ? resume.slots?.[resume.activeSlot] : null;
        if (resume?.slots) {
          setSlotInputs({ A: resume.slots.A ?? null, B: resume.slots.B ?? null });
          setActiveSlot(activeSlotName);
        }
        if (resume?.activeStage) setActiveStage(resume.activeStage); // else stays "fit"

        // §3.5 launch-from-cache: seed each slot's persisted fit into the backend so both
        // slots are hydrated (A/B switching is instant) and the active one can be written
        // *immediately*, skipping the ~1–2 s cold AutoEq fit. Falls back to a fresh fit
        // below when a slot has no cached fit (older resume blob / first run after update).
        const fits = resume?.fits;
        if (fits && useDev) {
          setSlotFits({ A: fits.A ?? null, B: fits.B ?? null });
          for (const s of ["A", "B"] as const) {
            const f = fits[s];
            if (!f) continue;
            await invoke("seed_slot", {
              slot: s,
              device: f.device,
              filters: f.filters,
              gTargetDb: f.g_target_db,
              gMaxPeakDb: f.g_max_peak_db,
              referenceCurve: f.reference_curve,
            });
            setHydrated((prev) => ({ ...prev, [s]: true }));
          }
        }
        const activeFit = activeSlotName !== "Dry" ? fits?.[activeSlotName] : null;

        if (resume && activeInp && useDev) {
          // Load the active slot's inputs into the controls. migrateInputs upgrades a
          // pre-stages blob (single customFilters → Tone stage) and backfills fixed macros.
          const inp = migrateInputs(activeInp);
          setQuery(inp.model);
          setMeasurementPath(inp.measurementPath);
          setTargetPath(inp.targetPath);
          setStages(inp.stages);
          try {
            if (activeFit) {
              // Fast path: the active slot was just seeded → write it from cache (instant),
              // then warm the sidecar's fit cache in the background so the first tone edit
              // doesn't pay the cold fit. The warm is fire-and-forget (no write, no race).
              const applied = await invoke<ApplyResult>("activate_slot", { slot: activeSlotName });
              setResult(applied);
              void invoke("warm_fit", {
                device: useDev.eqapo_pattern,
                headphone: inp.measurementPath,
                target: inp.targetPath || null,
              }).catch(() => {});
            } else {
              // No cached fit for the active slot: re-fit + write as before.
              setResult(await writeFit(activeSlotName as "A" | "B", inp, useDev));
            }
          } catch (e) {
            setError(String(e));
          }
        } else {
          // No resume (or active slot was empty/Dry): pre-fill the pickers from the older
          // last-selection, Harman as the default target.
          const sel = await invoke<Selection>("get_selection");
          const savedHp = sel.headphone ? hp.headphones.find((h) => h.path === sel.headphone) : undefined;
          if (savedHp) {
            setQuery(savedHp.name);
            setMeasurementPath(savedHp.path);
          }
          const savedTarget = sel.target && tg.targets.some((t) => t.path === sel.target) ? sel.target : undefined;
          const harman = tg.targets.find((t) => /harman over-ear 2018$/i.test(t.name));
          setTargetPath(savedTarget ?? harman?.path ?? tg.targets[0]?.path ?? "");
        }
      } catch (e) {
        setError(String(e));
      } finally {
        setRestored(true); // from here on, session changes persist to the resume blob
        setLoading(false);
      }
    })();
  }, []);

  // §3.5: persist the resume blob whenever the editable session changes (debounced).
  // Gated on `restored` so the initial defaults don't clobber the saved session first.
  useEffect(() => {
    if (!restored) return;
    const id = window.setTimeout(() => {
      const resume: Resume = {
        activeSlot,
        deviceId,
        slots: { A: slotInputs.A, B: slotInputs.B },
        fits: { A: slotFits.A, B: slotFits.B }, // §3.5: persist the fits for launch-from-cache
        activeStage,
      };
      invoke("set_resume", { resume }).catch(() => {});
    }, 400);
    return () => window.clearTimeout(id);
  }, [restored, activeSlot, deviceId, slotInputs, slotFits, activeStage]);

  // §3.5: persist the preset library (saved presets + filter templates) on change, same
  // debounce + `restored` gate as the resume blob.
  useEffect(() => {
    if (!restored) return;
    const id = window.setTimeout(() => {
      invoke("set_library", { library }).catch(() => {});
    }, 400);
    return () => window.clearTimeout(id);
  }, [restored, library]);

  // Poll status so the fail-safe banner reflects live watchdog health (trip/recover).
  useEffect(() => {
    const id = setInterval(() => {
      invoke<Status>("status").then(setStatus).catch(() => {});
    }, 3000);
    return () => clearInterval(id);
  }, []);

  // §5.2 measurement nerd overlays: fetch the raw + target curves when the measurement or
  // target changes (they share the dBr reference). Cleared on Dry / no measurement.
  useEffect(() => {
    if (activeSlot === "Dry" || !measurementPath) {
      setRawCurve(null);
      setTargetCurve(null);
      return;
    }
    let cancelled = false;
    invoke<{ raw_curve: { f: number; db: number }[]; target_curve: { f: number; db: number }[] }>("measurement_curves", {
      headphone: measurementPath,
      target: targetPath || null,
    })
      .then((r) => {
        if (cancelled) return;
        setRawCurve(r.raw_curve);
        setTargetCurve(r.target_curve?.length ? r.target_curve : null);
      })
      .catch(() => {
        if (cancelled) return;
        setRawCurve(null);
        setTargetCurve(null);
      });
    return () => {
      cancelled = true;
    };
  }, [measurementPath, targetPath, activeSlot]);

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

  // §4.2 clipping "add headroom": the base pre-gain that just clears the ceiling is
  // `-g_target - g_max_peak` (below that, the §4.1 loudness match binds again); round
  // down to the nearest 3 dB for a bit of margin, clamped to the input's -40..0 range.
  const headroomPregain =
    result && loudness ? Math.max(-40, Math.floor((-result.g_target_db - result.g_max_peak_db) / 3) * 3) : null;
  function addHeadroom() {
    if (!loudness || headroomPregain == null) return;
    void updateLoudness({ ...loudness, base_pregain_db: headroomPregain }); // a drop → direct write, no ramp
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
      const inp: SlotInputs = { model: query, measurementPath, targetPath, stages };
      setResult(await writeFit(activeSlot as "A" | "B", inp, dev));
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

  // Switch the active comparison slot. A hydrated A/B (fit cached this session) and Dry
  // write instantly (pure re-write, no re-fit); a slot whose inputs were restored from a
  // previous session but not yet fitted this session is re-fitted on first switch; an
  // empty A/B just becomes the editable target.
  async function switchSlot(slot: SlotName) {
    if (slot === activeSlot) return;
    setActiveSlot(slot);
    if (slot !== "Dry") {
      const raw = slotInputs[slot];
      const s = raw && migrateInputs(raw); // upgrade pre-stages blob + backfill macros
      if (s) {
        setQuery(s.model);
        setMeasurementPath(s.measurementPath);
        setTargetPath(s.targetPath);
        setStages(s.stages);
      }
      if (!s) return; // empty slot: nothing written yet, user will configure + apply
      if (!hydrated[slot]) {
        // Restored-but-not-fitted this session — re-fit it (a one-time cost per slot).
        const dev = devices.find((d) => d.id === deviceId);
        if (dev) {
          try {
            setError("");
            setApplying(true);
            setResult(await writeFit(slot, s, dev));
          } catch (e) {
            setError(String(e));
          } finally {
            setApplying(false);
          }
          return;
        }
      }
    }
    try {
      setError("");
      const applied = await invoke<ApplyResult>("activate_slot", { slot });
      setResult(applied);
    } catch (e) {
      setError(String(e));
    }
  }

  // Copy one editable slot onto the other and make it active — a starting point for
  // a variant (the target's cageq.txt is identical until you tweak it).
  async function copySlot(from: "A" | "B", to: "A" | "B") {
    const raw = slotInputs[from];
    const src = raw && migrateInputs(raw);
    if (!src) {
      setError(`Slot ${from} is empty — apply something to it first.`);
      return;
    }
    try {
      setError("");
      const applied = await invoke<ApplyResult>("copy_slot", { from, to });
      setSlotInputs((prev) => ({ ...prev, [to]: src }));
      setHydrated((prev) => ({ ...prev, [to]: true }));
      setActiveSlot(to);
      setQuery(src.model);
      setMeasurementPath(src.measurementPath);
      setTargetPath(src.targetPath);
      setStages(src.stages);
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

  // §3.4 tone editing — every change auto-applies (throttled). Edits target the *active
  // stage's* band list (the grid + chart nodes show only that stage); indices are into it.
  const setStageBands = (stage: StageId, updater: (bands: CustomFilter[]) => CustomFilter[]) =>
    setStages((st) => ({ ...st, [stage]: { ...st[stage], bands: updater(st[stage].bands) } }));
  // Mark a freshly-appended band (its storage index is the pre-append length) so the chart
  // pulses it — and, when `focusGrid`, the grid reveals + edit-focuses it. Auto-clears below.
  const markNewBand = (idx: number, focusGrid: boolean) => {
    newBandNonce.current += 1;
    setNewBand({ stage: activeStage, idx, nonce: newBandNonce.current, focusGrid });
  };
  // Double-click on the chart: create a peaking band exactly at the cursor (x → fc, y → gain),
  // so it lands where you're looking rather than at a fixed 1 kHz you then have to find. Mouse
  // path → pulse only, no grid focus-steal (you stay on the chart to drag/wheel it).
  const addFilterAt = (freq_hz: number, gain_db: number) => {
    const idx = stages[activeStage].bands.length;
    setStageBands(activeStage, (cf) => [...cf, { kind: "Peaking", freq_hz, gain_db, q: 1 }]);
    markNewBand(idx, false);
    requestApply(0);
  };
  // The Add button (keyboard/discoverability path): drop a flat band into the widest empty
  // gap between existing bands (in log-frequency), so it never lands on top of a neighbour,
  // and open its Fc for typing straight away (keyboard-first).
  const addFilter = () => {
    const bands = stages[activeStage].bands;
    const idx = bands.length;
    setStageBands(activeStage, (cf) => [...cf, { kind: "Peaking", freq_hz: widestGapHz(bands), gain_db: 0, q: 1 }]);
    markNewBand(idx, true);
    requestApply(0);
  };
  const updateFilter = (i: number, patch: Partial<CustomFilter>, delay = 0) => {
    setStageBands(activeStage, (cf) => cf.map((f, j) => (j === i ? { ...f, ...patch } : f)));
    requestApply(delay);
  };
  const removeFilter = (i: number) => {
    // The fixed Bass/Treble/Air macros are never removable (the grid hides their ✕; the
    // chart's double-click-to-remove would otherwise slip past that). Guard at the source.
    if (stages[activeStage].bands[i]?.fixed) return;
    setStageBands(activeStage, (cf) => cf.filter((_, j) => j !== i));
    setNewBand(null); // storage indices shift on removal — drop any stale highlight
    requestApply(0);
  };
  // The born-highlighted state is a one-shot cue: let the pulse play, then clear it (also
  // avoids a stale index lingering after later edits/reorders).
  useEffect(() => {
    if (!newBand) return;
    const t = window.setTimeout(() => setNewBand(null), 1500);
    return () => window.clearTimeout(t);
  }, [newBand]);
  // Enable/disable a whole stage — a big tonal jump the §5.3a morph smooths.
  const toggleStage = (id: StageId) => {
    setStages((st) => ({ ...st, [id]: { ...st[id], enabled: !st[id].enabled } }));
    requestApply(0);
  };
  // --- §3.5 preset library: load / save / delete -----------------------------
  // Loading always targets the *active* slot (disabled on Dry). A filter template drops its
  // bands into *its* stage (and switches to that tab); a full preset replaces measurement +
  // target + all stages.
  const loadTemplate = (t: FilterTemplate) => {
    if (activeSlot === "Dry") return;
    const bands = t.stage === "tone" ? ensureMacros(t.bands) : t.bands;
    setStages((st) => ({ ...st, [t.stage]: { enabled: true, bands } }));
    setActiveStage(t.stage);
    requestApply(0);
  };
  const loadPreset = (p: UserPreset) => {
    if (activeSlot === "Dry") return;
    setQuery(p.model);
    setMeasurementPath(p.measurementPath);
    setTargetPath(p.targetPath);
    setStages(normalizeStages(p.stages));
    requestApply(0);
  };

  // Save the current controls as a preset or template. Empty name → inline field error.
  // A name collision with the user's own entry of the same kind asks before overwriting
  // (replacing that entry in place, same id), so re-saving a tweaked preset doesn't spawn
  // a confusing duplicate; curated built-ins live in a separate namespace and never collide.
  function commitSave() {
    if (!saveForm) return;
    const name = saveForm.name.trim();
    if (!name) return setSaveForm({ ...saveForm, error: true });
    const kind = saveForm.kind;
    // A template collides only with an own template of the same name *in the same stage*
    // (a "Warm" Fit and a "Warm" Tone template are distinct); a preset collides by name.
    const existing =
      kind === "preset"
        ? library.presets.find((p) => p.name.toLowerCase() === name.toLowerCase())
        : library.templates.find((t) => t.stage === activeStage && t.name.toLowerCase() === name.toLowerCase());
    const id = existing?.id ?? crypto.randomUUID();
    const write = () => {
      setLibrary((lib) =>
        kind === "preset"
          ? { ...lib, presets: upsert(lib.presets, { id, name, model: query, measurementPath, targetPath, stages }) }
          : { ...lib, templates: upsert(lib.templates, { id, name, stage: activeStage, bands: stages[activeStage].bands }) },
      );
      setSaveForm(null);
    };
    if (existing) {
      setConfirmBox({
        message:
          kind === "preset"
            ? `A preset named “${name}” already exists. Overwrite it?`
            : `A ${STAGE_META[activeStage].label} template named “${name}” already exists. Overwrite it?`,
        confirmLabel: "Overwrite",
        onConfirm: write,
      });
    } else {
      write();
    }
  }

  // Overwrite an existing saved entry in place with the *current* controls (same id +
  // name) — the per-row "update" button, so tweaking a loaded preset and saving it back
  // doesn't mean retyping the name. Confirms first (reusing the overwrite dialog).
  const updatePreset = (p: UserPreset) =>
    setConfirmBox({
      message: `Overwrite the preset “${p.name}” with the current headphone, target and all stages?`,
      confirmLabel: "Overwrite",
      onConfirm: () =>
        setLibrary((lib) => ({
          ...lib,
          presets: upsert(lib.presets, { id: p.id, name: p.name, model: query, measurementPath, targetPath, stages }),
        })),
    });
  // A template row's "update" saves the current bands of *that template's* stage.
  const updateTemplate = (t: FilterTemplate) =>
    setConfirmBox({
      message: `Overwrite the ${STAGE_META[t.stage].label} template “${t.name}” with the current ${STAGE_META[t.stage].label} bands?`,
      confirmLabel: "Overwrite",
      onConfirm: () =>
        setLibrary((lib) => ({ ...lib, templates: upsert(lib.templates, { id: t.id, name: t.name, stage: t.stage, bands: stages[t.stage].bands }) })),
    });

  // Delete asks first — same confirm overlay as the other destructive actions.
  const deletePreset = (p: UserPreset) =>
    setConfirmBox({
      message: `Delete the preset “${p.name}”? This can't be undone.`,
      confirmLabel: "Delete",
      onConfirm: () => setLibrary((lib) => ({ ...lib, presets: lib.presets.filter((x) => x.id !== p.id) })),
    });
  const deleteTemplate = (t: FilterTemplate) =>
    setConfirmBox({
      message: `Delete the filter template “${t.name}”? This can't be undone.`,
      confirmLabel: "Delete",
      onConfirm: () => setLibrary((lib) => ({ ...lib, templates: lib.templates.filter((x) => x.id !== t.id) })),
    });

  // A/S/D switch slots, W toggles loudness mode — but not while typing in a field
  // (§5.2 blind-comparison shortcuts).
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

  // The active stage's bands (the grid/nodes edit these) and the full applied custom set.
  const activeBands = stages[activeStage].bands;
  const appliedCustom = useMemo(() => appliedBands(stages), [stages]);
  // Library filtering: templates for the active stage unless "show all" is on; curated
  // shapes are all Tone-staged, so they only show on the Tone tab (or with show-all).
  const visibleTemplates = showAllStages ? library.templates : library.templates.filter((t) => t.stage === activeStage);
  const showCurated = showAllStages || activeStage === "tone";

  // The active slot's composed bands split into its AutoEq fit and the custom tail. The
  // sidecar appends the (summed) custom filters after the AutoEq bands, so the fit is
  // everything before that tail. Drawn as fixed diamonds; the tone bands stay draggable.
  const autoEqBands = useMemo(() => {
    if (!result || dryActive) return [];
    const n = Math.max(0, result.filters.length - appliedCustom.length);
    return result.filters.slice(0, n);
  }, [result, appliedCustom, dryActive]);

  // §5.2 chart: only the *active* slot's total (drawing every slot at once crowded the
  // legend once the per-stage lines were added — the A/B comparison is primarily by ear).
  // Plus a line per enabled stage — the active stage prominent (it carries the drag nodes),
  // the others muted context. Stable ids so legend toggles survive switching slots/stages.
  const chartSeries: Series[] = useMemo(() => {
    if (!result) return [];
    const out: Series[] = [
      {
        id: `slot-${activeSlot}`,
        bands: result.filters,
        color: SLOT_COLOR[activeSlot],
        label: dryActive ? "Dry" : `Slot ${activeSlot}`,
      },
    ];
    if (!dryActive) {
      // Draw every enabled, non-empty stage — the SAME set regardless of which tab is active.
      // (Previously the active stage was special-cased and drawn even when empty, so e.g. an
      // empty Content stage appeared only on its own tab: the inconsistency.) The active stage
      // is prominent; the others are muted context.
      for (const id of STAGE_ORDER) {
        const st = stages[id];
        const bands = st.bands.filter((b) => b.enabled !== false);
        if (st.enabled && bands.length) {
          out.push({ id: `stage-${id}`, bands, color: STAGE_COLOR[id], label: STAGE_META[id].label, muted: id !== activeStage });
        }
      }
    }
    return out;
  }, [result, activeSlot, dryActive, stages, activeStage]);

  const chartMarkers: Marker[] = useMemo(
    // Off by default — a nerd overlay revealed from the legend.
    () => (autoEqBands.length ? [{ id: "autoeq", bands: autoEqBands, color: SLOT_COLOR[activeSlot], label: "AutoEq", defaultHidden: true }] : []),
    [autoEqBands, activeSlot],
  );

  // The ideal correction the active slot's fit chases (§5.2): the AutoEq curve should
  // hug it; the gap is the residual the parametric fit couldn't capture. Off for Dry.
  const chartRefs: RefCurve[] = useMemo(() => {
    if (dryActive) return [];
    const out: RefCurve[] = [];
    if (result?.reference_curve?.length)
      out.push({ id: "ideal", points: result.reference_curve, color: REF_COLOR, label: "Ideal EQ" });
    // Target + raw measurement — nerd overlays, off by default (share the dBr reference).
    if (targetCurve?.length)
      out.push({ id: "target", points: targetCurve, color: TARGET_COLOR, label: "Target", defaultHidden: true });
    if (rawCurve?.length)
      out.push({ id: "raw", points: rawCurve, color: RAW_COLOR, label: "Raw", defaultHidden: true });
    return out;
  }, [result, dryActive, rawCurve, targetCurve]);

  // Phase of the applied filter chain, on the secondary axis — a nerd overlay, off by default.
  const chartPhase: PhaseCurve | undefined = useMemo(
    () => (!dryActive && result ? { id: "phase", bands: result.filters, color: PHASE_COLOR, label: "Phase", defaultHidden: true } : undefined),
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
          <div className="modal-card" onClick={(e) => e.stopPropagation()}>
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

      {confirmBox && (
        <div
          onClick={() => setConfirmBox(null)}
          style={{ position: "fixed", inset: 0, background: "#0006", display: "flex", alignItems: "center", justifyContent: "center", zIndex: 10 }}
        >
          <div className="modal-card" onClick={(e) => e.stopPropagation()}>
            <p style={{ marginTop: 0 }}>{confirmBox.message}</p>
            <div className="row" style={{ justifyContent: "flex-end", gap: "0.5em" }}>
              <button type="button" onClick={() => setConfirmBox(null)}>
                Cancel
              </button>
              <button
                type="button"
                onClick={() => {
                  confirmBox.onConfirm();
                  setConfirmBox(null);
                }}
              >
                {confirmBox.confirmLabel}
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
              <label style={{ fontSize: "0.85em", opacity: 0.75 }}>Measurement</label>
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
          <span className="info-chip" tabIndex={0} role="button" aria-label="Diagnostics">
            ⓘ
            <span className="info-pop">
              {status.sidecar} · {status.health}
              <br />
              {status.config_source}
            </span>
          </span>
        )}
      </header>

      {!loading && selectedDevice && !selectedDevice.eqapo_enabled && (
        <p style={{ color: "#b8860b", fontSize: "0.85em", margin: "0 0 0.8em" }}>
          ⚠ Equalizer APO isn't installed on this device, so applying an EQ here has no effect. Enable it
          for this device with Equalizer APO's <em>Configurator</em> (DeviceSelector.exe), then reboot.
        </p>
      )}

      {/* The cold-start wait is the Python DSP sidecar: after a reboot its numpy/scipy bundle
          is read cold from disk (a couple of seconds; the OS file cache makes repeat launches
          fast) plus the one-time import — I/O-bound, not the catalogue (a 17 ms read). */}
      {loading && <p>Starting the AutoEq engine…</p>}

      <div className="app-main">
        {/* ================= LEFT: target + chart + bands ================= */}
        <section>
          {!loading && (
            <div className="panel">
              <div className="row" style={{ justifyContent: "space-between", alignItems: "baseline", marginBottom: "0.35rem" }}>
                <h2 style={{ margin: 0 }}>Correction</h2>
                {result && !dryActive && (
                  <div className="chart-view" style={{ margin: 0 }} role="group" aria-label="Chart domain">
                    <button
                      type="button"
                      className={!impulseView ? "on" : ""}
                      title="Frequency domain — magnitude (and phase)"
                      onClick={() => setImpulseView(false)}
                    >
                      Frequency
                    </button>
                    <button
                      type="button"
                      className={impulseView ? "on" : ""}
                      title="Time domain — impulse-response decay"
                      onClick={() => setImpulseView(true)}
                    >
                      Time
                    </button>
                  </div>
                )}
              </div>
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

              {result && (
                <>
                  {/* The Frequency/Time (domain) view swap lives in the panel header row
                      (above). Phase rides the frequency chart's secondary axis (legend). */}
                  <div className="chart-wrap">
                    {impulseView && !dryActive ? (
                      <ImpulseChart bands={result.filters} color={SLOT_COLOR[activeSlot]} height={215} />
                    ) : (
                      <EqChart
                        series={chartSeries}
                        markers={chartMarkers}
                        refs={chartRefs}
                        phase={chartPhase}
                        height={215}
                        nodes={{
                          bands: activeBands,
                          color: STAGE_COLOR[activeStage],
                          disabled: dryActive,
                          onChange: (i, patch) => updateFilter(i, patch, 70),
                          onDragEnd: () => requestApply(0),
                          onAdd: addFilterAt,
                          onRemove: removeFilter,
                          highlightIdx: newBand?.stage === activeStage ? newBand.idx : undefined,
                          hoverIdx: hoverBand,
                          onHover: setHoverBand,
                        }}
                      />
                    )}
                    <div
                      className="chart-preamp"
                      title={
                        loudness?.mode === "FinalVolume"
                          ? "Preamp for maximum clipping-free volume"
                          : "Preamp for the loudness-matched level (Auto-LUFS)"
                      }
                    >
                      Preamp <b>{result.preamp_db.toFixed(1)} dB</b>
                      <span className="chart-preamp-mode">{loudness?.mode === "FinalVolume" ? "max" : "matched"}</span>
                    </div>
                  </div>
                  {result.clipping_warning && (
                    <p style={{ color: "#b8860b", fontSize: "0.8em", margin: "0.2em 0 0" }}>
                      ⚠ Emergency clipping protection active instead of the loudness match — extreme peak.
                      {loudness?.mode === "Comparison" && headroomPregain != null && headroomPregain < loudness.base_pregain_db && (
                        <button
                          type="button"
                          onClick={addHeadroom}
                          style={{ marginLeft: "0.5em", fontSize: "0.9em", padding: "0.1em 0.5em" }}
                          title="Lower the base pre-gain so the loudness match fits instead of the clipping ceiling"
                        >
                          Add headroom → {headroomPregain} dB
                        </button>
                      )}
                    </p>
                  )}
                </>
              )}
            </div>
          )}

          {/* ---- filter bands: per-stage keyboard-first graphic-EQ grid (§5.2 stage 3) ---- */}
          {!loading && (
            <div className="panel" style={{ opacity: dryActive ? 0.5 : 1 }}>
              <h2>Filter bands</h2>
              {dryActive ? (
                <p className="tg-empty">Dry is the fixed reference — pick Slot A or B to edit filter bands.</p>
              ) : (
                <>
                  {/* Stage selector (segmented): the name button picks the stage to edit; the
                      power icon toggles the whole stage on/off. Active = tinted in the stage colour. */}
                  <div className="stage-tabs" role="tablist" aria-label="Filter stages">
                    {STAGE_ORDER.map((id) => {
                      const st = stages[id];
                      const isActive = id === activeStage;
                      const count = st.bands.filter((b) => b.enabled !== false).length;
                      const color = STAGE_COLOR[id];
                      return (
                        <div
                          key={id}
                          className={`stage-seg${isActive ? " active" : ""}${st.enabled ? "" : " off"}`}
                          style={isActive ? { background: `color-mix(in srgb, ${color} 15%, transparent)`, boxShadow: `inset 0 -2px 0 ${color}`, color } : undefined}
                        >
                          <button
                            type="button"
                            role="tab"
                            aria-selected={isActive}
                            className="stage-seg-select"
                            title={STAGE_META[id].hint}
                            onClick={() => setActiveStage(id)}
                          >
                            {STAGE_META[id].label}
                            {count > 0 && <span className="stage-count">{count}</span>}
                          </button>
                          <button
                            type="button"
                            className="stage-seg-power"
                            role="switch"
                            aria-checked={st.enabled}
                            title={st.enabled ? `Disable the ${STAGE_META[id].label} stage` : `Enable the ${STAGE_META[id].label} stage`}
                            aria-label={`${st.enabled ? "Disable" : "Enable"} the ${STAGE_META[id].label} stage`}
                            onClick={() => toggleStage(id)}
                          >
                            <PowerGlyph />
                          </button>
                        </div>
                      );
                    })}
                  </div>
                  <p className="stage-hint">
                    {STAGE_META[activeStage].hint}
                    {!stages[activeStage].enabled && <b> · stage disabled (not applied)</b>}
                  </p>

                  <ToneGrid
                    filters={activeBands}
                    disabled={dryActive}
                    focusIndex={newBand?.stage === activeStage && newBand.focusGrid ? newBand.idx : null}
                    focusNonce={newBand?.nonce}
                    hoverIndex={hoverBand}
                    onHover={setHoverBand}
                    onInput={(i, patch) => updateFilter(i, patch, 70)}
                    onCommit={(i, patch) => updateFilter(i, patch, 0)}
                    onAdd={addFilter}
                    onRemove={removeFilter}
                  />
                  {/* Always rendered (with reserved height) so switching to an empty stage
                      doesn't shrink the panel; the text just adapts to empty vs populated. */}
                  <p className="tg-hint">
                    {activeBands.length > 0
                      ? "Drag a value to scrub, click to type, ↑/↓ to fine-tune · double-click the chart to add a band (or a node to remove it) · changes apply live."
                      : "This stage has no bands yet — press ＋ or double-click the chart to add one."}
                  </p>
                </>
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
              <h2>Loudness</h2>
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
              <div className="row" style={{ justifyContent: "space-between", alignItems: "baseline" }}>
                <h2 style={{ margin: 0 }}>Presets &amp; filters</h2>
                <button
                  type="button"
                  className="pl-save"
                  disabled={dryActive}
                  onClick={() =>
                    setSaveForm(saveForm ? null : { kind: measurementPath ? "preset" : "template", name: "", error: false })
                  }
                  title="Save the current setup or tone"
                >
                  <span aria-hidden>💾</span> Save
                </button>
              </div>

              {saveForm && (
                <div className="pl-saveform">
                  <div className="pl-toggle">
                    <button type="button" className={saveForm.kind === "template" ? "on" : ""} onClick={() => setSaveForm({ ...saveForm, kind: "template" })}>
                      Filter template
                    </button>
                    <button
                      type="button"
                      className={saveForm.kind === "preset" ? "on" : ""}
                      disabled={!measurementPath}
                      title={measurementPath ? undefined : "Pick a headphone to save a full preset"}
                      onClick={() => setSaveForm({ ...saveForm, kind: "preset" })}
                    >
                      Full preset
                    </button>
                  </div>
                  <p className="pl-hint">
                    {saveForm.kind === "preset"
                      ? "Saves the measurement, target curve and all filter stages — loading replaces the whole setup."
                      : `Saves the current ${STAGE_META[activeStage].label} stage's bands — loading drops them into ${STAGE_META[activeStage].label}, keeping measurement & target.`}
                  </p>
                  <div className="row" style={{ gap: "0.3em" }}>
                    <input
                      type="text"
                      placeholder="Name"
                      autoFocus
                      value={saveForm.name}
                      onChange={(e) => setSaveForm({ ...saveForm, name: e.currentTarget.value, error: false })}
                      onKeyDown={(e) => {
                        if (e.key === "Enter") commitSave();
                        else if (e.key === "Escape") setSaveForm(null);
                      }}
                      style={{ flex: 1, borderColor: saveForm.error ? "#c0392b" : undefined }}
                    />
                    <button type="button" onClick={commitSave}>
                      Save
                    </button>
                    <button type="button" onClick={() => setSaveForm(null)}>
                      Cancel
                    </button>
                  </div>
                </div>
              )}

              <h3 className="pl-group">
                Filter templates <span>{showAllStages ? "all stages" : STAGE_META[activeStage].label}</span>
                <button type="button" className="pl-showall" onClick={() => setShowAllStages((v) => !v)}>
                  {showAllStages ? "active stage" : "show all"}
                </button>
              </h3>
              {showCurated && (
                <div className="row" style={{ gap: "0.35em" }}>
                  {CURATED_TEMPLATES.map((t) => (
                    <button key={t.id} type="button" disabled={dryActive} onClick={() => loadTemplate(t)} style={{ fontSize: "0.8em" }}>
                      {t.name}
                    </button>
                  ))}
                </div>
              )}
              {visibleTemplates.length > 0 && (
                <ul className="pl-list">
                  {visibleTemplates.map((t) => (
                    <li key={t.id} className="pl-item">
                      {showAllStages && (
                        <span className="stage-badge" style={{ color: STAGE_COLOR[t.stage] }}>
                          {STAGE_META[t.stage].label}
                        </span>
                      )}
                      <span className="pl-name" title={t.name}>
                        {t.name}
                      </span>
                      <button type="button" disabled={dryActive} onClick={() => loadTemplate(t)}>
                        Load
                      </button>
                      <button
                        type="button"
                        className="pl-upd"
                        title={`Overwrite with the current ${STAGE_META[t.stage].label} bands`}
                        disabled={dryActive}
                        onClick={() => updateTemplate(t)}
                      >
                        💾
                      </button>
                      <button type="button" className="pl-del" title="Delete" onClick={() => deleteTemplate(t)}>
                        🗑
                      </button>
                    </li>
                  ))}
                </ul>
              )}

              <h3 className="pl-group">
                Presets <span>measurement + target + all stages</span>
              </h3>
              {library.presets.length > 0 ? (
                <ul className="pl-list">
                  {library.presets.map((p) => (
                    <li key={p.id} className="pl-item">
                      <span className="pl-name" title={`${p.name} — ${p.model || "no measurement"}`}>
                        {p.name}
                      </span>
                      <button type="button" disabled={dryActive} onClick={() => loadPreset(p)}>
                        Load
                      </button>
                      <button
                        type="button"
                        className="pl-upd"
                        title={measurementPath ? "Overwrite with the current setup" : "Pick a headphone to overwrite this preset"}
                        disabled={dryActive || !measurementPath}
                        onClick={() => updatePreset(p)}
                      >
                        💾
                      </button>
                      <button type="button" className="pl-del" title="Delete" onClick={() => deletePreset(p)}>
                        🗑
                      </button>
                    </li>
                  ))}
                </ul>
              ) : (
                <p className="pl-empty">Pick a headphone + target, then 💾 Save a full preset to recall the whole setup.</p>
              )}
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

      <footer className="app-footer">
        Headphone corrections and target curves from{" "}
        <a
          href="https://github.com/jaakkopasanen/AutoEq"
          onClick={(e) => {
            e.preventDefault();
            void openUrl("https://github.com/jaakkopasanen/AutoEq").catch(() => {});
          }}
        >
          AutoEq
        </a>{" "}
        by Jaakko Pasanen, MIT-licensed. CAGEq is an independent project, not affiliated with AutoEq.
      </footer>
    </main>
  );
}

export default App;
