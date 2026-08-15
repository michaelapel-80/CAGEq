import { useEffect, useMemo, useRef, useState, type CSSProperties } from "react";
import { useTranslation, Trans } from "react-i18next";
import { invoke } from "@tauri-apps/api/core";
import { listen, emit } from "@tauri-apps/api/event";
import { getVersion } from "@tauri-apps/api/app";
import { WebviewWindow } from "@tauri-apps/api/webviewWindow";
import { openUrl } from "@tauri-apps/plugin-opener";
import { LANGS, setLang, type LangCode } from "./i18n";
import { Band, composedCurveDb, logGrid } from "./biquad";
import { EqChart, Marker, PhaseCurve, RefCurve, Series, SpectrumData } from "./EqChart";
import { ImpulseChart } from "./NerdCharts";
import { ToneGrid } from "./ToneGrid";
import { ScrubNumber } from "./ScrubNumber";
import { Meter } from "./Meter";
import { Vectorscope } from "./Vectorscope";
import { TimeScope } from "./TimeScope";
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
// Mirrors biquad's FilterKind. "Bandpass" only ever appears in the §5.2 isolate *result* (drawn on
// the chart), never as an editable band — the grid cycles just Peaking/LowShelf/HighShelf.
type FilterKind = "Peaking" | "LowShelf" | "HighShelf" | "Bandpass";
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
  slotPreset?: { A: LoadedRef | null; B: LoadedRef | null }; // which preset each slot was loaded from
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
// Stage display labels/hints are localized — see the `stageLabel`/`stageHint` helpers (i18n keys
// `stages.<id>.label` / `.hint`). The ids (fit/content/tone) stay English internally.

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

/** Deterministic JSON with object keys sorted recursively (arrays keep their order). So the
 *  same *values* always produce the same string regardless of key insertion order — which
 *  differs between a fresh in-memory object and one reparsed from the persisted resume blob
 *  (serde may reorder map keys). Lets a stored signature compare by value, not by text. */
function stableStringify(v: unknown): string {
  if (v === null || typeof v !== "object") return JSON.stringify(v);
  if (Array.isArray(v)) return `[${v.map(stableStringify).join(",")}]`;
  const o = v as Record<string, unknown>;
  return `{${Object.keys(o).sort().map((k) => `${JSON.stringify(k)}:${stableStringify(o[k])}`).join(",")}}`;
}

/** A stable fingerprint of a slot's *document* (model + measurement + target + all stages),
 *  used to tell whether the editor has diverged from the preset last loaded into a slot
 *  (the "dirty" flag). Compared against the sig captured at load time — see `slotPreset`.
 *  Canonical (sorted-key) so it survives a persist→reload round-trip unchanged. */
const presetSig = (model: string, measurementPath: string, targetPath: string, stages: Stages) =>
  stableStringify({ model, measurementPath, targetPath, stages });

/** Which preset (or archived version) a slot was last loaded from, plus the document sig at
 *  load time — so the slot can show the preset name and flag divergence. Persisted in the resume
 *  blob; `sig` is the clean baseline for the dirty check. `at` identifies *which* version was
 *  loaded — `"head"` for the preset's current state, or an archived version's own timestamp —
 *  rather than caching its vN label: the label is position-based (v1, v2, …) and every other
 *  version's position shifts whenever one is deleted, so a cached string goes stale the moment
 *  that happens elsewhere. The label is derived live from `library` at render time instead (see
 *  `presetVerLabel`), so it always matches what the versions list itself shows. */
type LoadedRef = { id: string; name: string; at?: number | "head"; sig: string };

/** The custom bands actually written: every enabled stage's enabled bands, in stage order —
 *  what rides to the sidecar (appended to the AutoEq fit) and drives the §4.1/§4.2 policy. */
const appliedBands = (st: Stages): CustomFilter[] =>
  STAGE_ORDER.flatMap((id) => (st[id].enabled ? st[id].bands.filter((b) => b.enabled !== false) : []));

/** §5.2 solo: which band, in which stage, is soloed (hear only it, within its stage). */
type Solo = { stage: StageId; idx: number };
/** Apply a solo to a stage set for the *applied* cascade only — never mutates the stored bands.
 *  Forces the soloed band's stage on and every other band in that stage off; other stages are
 *  untouched. The §4.1 loudness match then recomputes the preamp for the reduced cascade (auto-gain). */
function soloStages(stages: Stages, solo: Solo | null): Stages {
  if (!solo) return stages;
  const st = stages[solo.stage];
  if (!st || solo.idx < 0 || solo.idx >= st.bands.length) return stages;
  return {
    ...stages,
    [solo.stage]: { enabled: true, bands: st.bands.map((b, i) => ({ ...b, enabled: i === solo.idx })) },
  };
}

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
  versions?: (Partial<PresetState> & { at?: number })[];
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
    versions: (p.versions ?? []).map((v) => ({
      at: v.at ?? 0,
      model: v.model ?? p.model,
      measurementPath: v.measurementPath ?? p.measurementPath,
      targetPath: v.targetPath ?? p.targetPath,
      stages: normalizeStages(v.stages),
    })),
  }));
  return { presets, templates };
}

// Presets are bass/treble/air gain triples applied to the three fixed macro bands;
// picking one resets the tone to exactly those three bands at the given gains.
// `name` is the stable identity (feeds the curated template id, persisted); `key` is its i18n
// label key — so the display name localizes without changing the id.
const TONE_PRESETS: { name: string; key: string; bass: number; treble: number; air: number }[] = [
  { name: "Flat", key: "flat", bass: 0, treble: 0, air: 0 },
  { name: "Bass boost", key: "bassBoost", bass: 6, treble: 0, air: 0 },
  { name: "Treble boost", key: "trebleBoost", bass: 0, treble: 5, air: 0 },
  { name: "Airy", key: "airy", bass: 0, treble: 0, air: 5 },
  { name: "V-shape", key: "vShape", bass: 5, treble: 4, air: 2 },
  { name: "Warm", key: "warm", bass: 4, treble: -3, air: -3 },
  { name: "Bright", key: "bright", bass: -2, treble: 4, air: 3 },
];
// name → i18n label key, for translating the curated templates' display names.
const TONE_PRESET_KEY: Record<string, string> = Object.fromEntries(TONE_PRESETS.map((p) => [p.name, p.key]));

// §3.5 preset library. Two kinds, deliberately different in scope (filter.md §5.2):
//   • FilterTemplate — one **stage's** bands; loading drops them into that stage only.
//   • UserPreset     — a full setup (measurement + target + all stages); loading replaces all.
// Both persist in settings.json's opaque `library` blob (get_library/set_library), the
// same UI-owned-blob treatment as the resume state — no per-field Rust change.
type FilterTemplate = { id: string; name: string; stage: StageId; bands: CustomFilter[] };
// The loadable payload shared by a preset and its archived versions (everything a full load needs).
type PresetState = { model: string; measurementPath: string; targetPath: string; stages: Stages };
// A past state of a preset, kept when the user explicitly saves a new version. `at` = timestamp.
type PresetVersion = PresetState & { at: number };
// A UserPreset is its *current* state plus a linear history of explicitly-saved previous versions
// (§3.5 versioning). Newest archived version is last. Loading a version drops it into the active
// slot like a preset — the built-in level-matched A/B does the actual comparison.
type UserPreset = { id: string; name: string; versions?: PresetVersion[] } & PresetState;
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
// The read-only AutoEq-fit stage: a muted slate, distinct from the three editable stage hues —
// it isn't yours to tune, it's what AutoEq computed.
const AUTOEQ_COLOR = "#8b93a7";
const REF_COLOR = "#a855f7"; // AutoEq's ideal-correction reference (target the fit chases)
const RAW_COLOR = "#94a3b8"; // the raw headphone measurement (nerd overlay)
const TARGET_COLOR = "#7dd3fc"; // the target curve — pale blue, à la AutoEq (nerd overlay)
const PHASE_COLOR = "#f59e0b"; // the filter chain's phase, on the secondary axis (nerd overlay)

// §5.4 self-test verdict — **delta method**. We measure the loopback with the correction applied and
// again with it bypassed (Dry), and compare their *difference*: delta = corrected − dry. That delta
// is the EQ's own transfer function, because everything common to both captures — the pink source's
// imperfect flatness, any foreign config.txt filters, fixed system coloration — cancels exactly. So
// the leftover is purely CAGEq's contribution, which we correlate against the applied EQ curve.
type SelfTestVerdict =
  // `deviceColorDb` (on pass/mismatch, where a real Dry capture exists) is how far the Dry pink
  // noise deviates from flat — the device's *own* processing, which the delta method cancels. See
  // §5.4 dry-flatness below.
  | { kind: "pass"; r: number; deviceColorDb?: number }
  | { kind: "fail" }
  | { kind: "mismatch"; r: number; deviceColorDb?: number }
  | { kind: "nosignal" }
  | { kind: "inconclusive" };
// Residual RMS of `y` after removing its best-fit line vs `x` — i.e. how much the curve deviates
// from a straight (tilted) line. Used to judge Dry-pink flatness: a broadband tilt/gain is allowed
// (source slope, device volume), but spectral shaping (peaks/dips/roll-off) shows up in the residual.
function flatnessResidual(x: number[], y: number[]): number {
  const n = x.length;
  if (n < 3) return 0;
  const mx = x.reduce((a, v) => a + v, 0) / n;
  const my = y.reduce((a, v) => a + v, 0) / n;
  let sxx = 0;
  let sxy = 0;
  for (let k = 0; k < n; k++) {
    const dx = x[k] - mx;
    sxx += dx * dx;
    sxy += dx * (y[k] - my);
  }
  const slope = sxx > 0 ? sxy / sxx : 0;
  let ss = 0;
  for (let k = 0; k < n; k++) {
    const resid = y[k] - my - slope * (x[k] - mx);
    ss += resid * resid;
  }
  return Math.sqrt(ss / n);
}
/** Dry-pink deviation (dB RMS) above which the device is likely applying its own processing.
 *  An estimate — tune against known-transparent vs known-colouring (enhancements-on) devices. */
const DEVICE_COLOR_DB = 1.2;
type SelfTestState = { phase: "warn" } | { phase: "running" } | { phase: "done"; verdict: SelfTestVerdict };
const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

/** Average `frames`' per-bin dB into one Float64Array (empty → zeros). */
function avgSpectrum(frames: SpectrumData[], n: number): Float64Array {
  const a = new Float64Array(n);
  if (frames.length === 0) return a;
  for (const f of frames) for (let i = 0; i < n; i++) a[i] += f.db[i];
  for (let i = 0; i < n; i++) a[i] /= frames.length;
  return a;
}

function selfTestVerdict(corrected: SpectrumData[], dry: SpectrumData[], bands: Band[], fs?: number): SelfTestVerdict {
  // No (or barely any) captured frames ⇒ the loopback produced nothing — e.g. the device was
  // disabled/removed mid-test and the capture is stuck reopening. That's no-signal, not "too flat".
  if (corrected.length < 3 || dry.length < 3) return { kind: "nosignal" };
  const N = corrected[0].db.length;
  const Lc = avgSpectrum(corrected, N);
  const Ld = avgSpectrum(dry, N);
  const { f_min, f_max } = corrected[0];
  const freqs = new Float64Array(N);
  for (let i = 0; i < N; i++) freqs[i] = f_min * (f_max / f_min) ** (i / (N - 1));
  const C = composedCurveDb(bands, freqs, fs); // expected EQ magnitude at each bin (at the device rate)
  // Trust only bins with real energy in *both* captures (pink well above the display floor).
  const idx: number[] = [];
  for (let i = 0; i < N; i++) if (Lc[i] > -90 && Ld[i] > -90) idx.push(i);
  // Too few live bins ⇒ the test signal never really reached the loopback (wrong/muted/disabled
  // device) — that's "nothing vs nothing", not evidence the EQ is broken. Report it as such.
  if (idx.length < N / 2) return { kind: "nosignal" };
  const D = idx.map((i) => Lc[i] - Ld[i]); // measured EQ transfer function (dB)
  const Ci = idx.map((i) => C[i]);
  const n = idx.length;
  const meanD = D.reduce((a, v) => a + v, 0) / n; // constant offset (preamp difference) — irrelevant to shape
  const meanC = Ci.reduce((a, v) => a + v, 0) / n;
  let sDD = 0, sCC = 0, sDC = 0;
  for (let k = 0; k < n; k++) {
    const d = D[k] - meanD;
    const c = Ci[k] - meanC;
    sDD += d * d;
    sCC += c * c;
    sDC += d * c;
  }
  const stdD = Math.sqrt(sDD / n); // how much the measured EQ effect actually varies (dB RMS)
  const stdC = Math.sqrt(sCC / n); // how much the correction predicts
  // §5.4 dry-flatness: the generated pink reads flat in the log spectrum, so a *transparent* device
  // returns a flat Dry capture. Its deviation from flat (residual after a linear fit, so a tilt or
  // broadband gain doesn't count) is the device's own processing — the thing the delta method
  // cancels and can't see. Surfaced as an advisory alongside the EQ verdict.
  const deviceColorDb = flatnessResidual(idx.map((i) => Math.log(freqs[i])), idx.map((i) => Ld[i]));
  // NOTE: thresholds are estimates; want a little live tuning against known-good/known-broken setups.
  if (stdC < 1.5) return { kind: "inconclusive" }; // correction too flat to measure against
  if (stdD < 0.8) return { kind: "fail" }; // corrected ≈ dry → CAGEq's config isn't reaching the output
  const r = sDC / (Math.sqrt(sDD * sCC) || 1);
  // stdD can sit below stdC because the 120-bin spectrum smooths sharp high-Q filters, so allow that.
  if (r > 0.75 && stdD > 0.5 * stdC) return { kind: "pass", r, deviceColorDb };
  return { kind: "mismatch", r, deviceColorDb };
}

// The base pre-gain field, deliberately isolated from App state. Holding an arrow updates only this
// component's local value — the whole-App re-render (chart + meters + grid) that `loudness` state
// would trigger on every step is what stalled the UI thread. It commits to the parent (which owns
// the backend apply) only when you settle (debounced) or on blur/Enter. `value` re-syncs the local
// value on an *external* change (addHeadroom, restore) but ignores the echo of our own commit.
function PreampField({
  value,
  disabled,
  ariaLabel,
  onCommit,
}: {
  value: number;
  disabled: boolean;
  ariaLabel: string;
  onCommit: (db: number) => void;
}) {
  const [local, setLocal] = useState(value);
  const localRef = useRef(local);
  localRef.current = local;
  const lastCommit = useRef(value);
  const timer = useRef<number | null>(null);
  useEffect(() => {
    if (value !== lastCommit.current) {
      setLocal(value);
      lastCommit.current = value;
    }
  }, [value]);
  const schedule = (delay: number) => {
    if (timer.current != null) window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => {
      timer.current = null;
      lastCommit.current = localRef.current;
      onCommit(localRef.current);
    }, delay);
  };
  return (
    <ScrubNumber
      value={local}
      min={-40}
      max={0}
      mode="add"
      arrowStep={1}
      decimals={0}
      disabled={disabled}
      ariaLabel={ariaLabel}
      style={{ width: "2em", textAlign: "right" }}
      onInput={(v) => {
        setLocal(v);
        schedule(180);
      }}
      onCommit={(v) => {
        setLocal(v);
        schedule(0);
      }}
    />
  );
}

function App() {
  // i18n aliased to `tr` (App.tsx already uses `t` as a lambda param for templates/targets).
  const { t: tr, i18n } = useTranslation();
  // Localized display helpers for the identity-keyed enums (stage/slot stay English ids internally).
  const stageLabel = (id: StageId) => tr(`stages.${id}.label`);
  const stageHint = (id: StageId) => tr(`stages.${id}.hint`);
  const slotLabel = (s: SlotName) => tr(`slots.${s}`);
  const [status, setStatus] = useState<Status | null>(null);
  const [headphones, setHeadphones] = useState<Headphone[]>([]);
  const [targets, setTargets] = useState<Target[]>([]);
  const [devices, setDevices] = useState<AudioDevice[]>([]);
  const [deviceId, setDeviceId] = useState("");
  // True when the last session's output device isn't currently active (e.g. unplugged) and we fell
  // back to another endpoint on launch — surfaced as a hint, cleared once the user picks a device.
  const [resumeDeviceMissing, setResumeDeviceMissing] = useState(false);
  const [query, setQuery] = useState(""); // headphone-model search / selected model name
  const [measurementPath, setMeasurementPath] = useState(""); // chosen measurement (source) path
  const [targetPath, setTargetPath] = useState("");
  const [stages, setStages] = useState<Stages>(defaultStages()); // §3.4 the three per-slot filter stages
  const [activeStage, setActiveStage] = useState<StageId>("fit"); // which stage the grid/chart edits (restored from resume)
  // Read-only "AutoEq fit" tab in the band section: shows the automatic parametric fit (not one
  // of the editable stages). A view flag, not a StageId, so `stages[activeStage]` stays valid and
  // the chart's drag nodes keep tracking the last editable stage underneath.
  const [autoEqView, setAutoEqView] = useState(false);
  // A just-added band, born highlighted so it's never hunted for. The chart always pulses its
  // node; `focusGrid` additionally scrolls the grid column in and enters its Fc edit mode —
  // set only for the keyboard/Add path, so a mouse double-click on the chart isn't yanked off
  // the chart into the grid. `nonce` re-fires the effects even when the storage idx repeats.
  const [newBand, setNewBand] = useState<{ stage: StageId; idx: number; nonce: number; focusGrid: boolean } | null>(null);
  const newBandNonce = useRef(0);
  // Cross-view hover link: the storage index of the band the pointer is over in *either* the
  // chart or the grid, so the other view highlights the matching node/column (null = none).
  const [hoverBand, setHoverBand] = useState<number | null>(null);
  // Force light/dark to override the system preference (handy for testing both). `auto` follows
  // the OS. Persisted across reloads; applied as a `data-theme` attribute the CSS keys off.
  const [theme, setTheme] = useState<"auto" | "light" | "dark">(
    () => (localStorage.getItem("cageq-theme") as "auto" | "light" | "dark" | null) ?? "auto",
  );
  useEffect(() => {
    const el = document.documentElement;
    if (theme === "auto") el.removeAttribute("data-theme");
    else el.setAttribute("data-theme", theme);
    localStorage.setItem("cageq-theme", theme);
  }, [theme]);
  const cycleTheme = () => setTheme((t) => (t === "auto" ? "light" : t === "light" ? "dark" : "auto"));
  // Live post-EQ spectrum (loopback FFT) drawn on the chart. The Meter component owns start/stop
  // of the capture; here we just subscribe to the `spectrum` events it produces. A ref, not state:
  // this arrives at up to ~60 fps for as long as the app is open, and `setState` would re-render
  // the *entire* App tree (every panel, the grid, the library) on every frame indefinitely — real,
  // continuous work that a plain function component doesn't skip just because its own JSX output
  // doesn't end up changing. EqChart reads the ref itself via its own rAF loop (same pattern as
  // Vectorscope's `scopeRef`), so arrival never touches React at all.
  const spectrumRef = useRef<SpectrumData | null>(null);
  // The selected output's live mix sample rate (Hz), reported by the loopback monitor (§8 read-only
  // format display). Null when monitoring isn't running yet. Shown next to the device picker.
  const [sampleRate, setSampleRate] = useState<number | null>(null);
  useEffect(() => {
    let active = true;
    let unlisten: (() => void) | undefined;
    void (async () => {
      unlisten = await listen<SpectrumData>("spectrum", (e) => {
        if (active) spectrumRef.current = e.payload;
      });
    })();
    return () => {
      active = false;
      unlisten?.();
    };
  }, []);
  // Align the vertical meter bars to the chart's *plot area* (top gridline → X axis) rather than
  // letting them stretch past it (the chart-wrap also holds the legend). Measured near the return.
  const chartWrapRef = useRef<HTMLDivElement>(null);
  const [plotBox, setPlotBox] = useState<{ top: number; height: number } | null>(null);
  // The chart legend renders (via portal) into this full-width host *below* the chart-row, so its
  // toggle chips can use the whole width (incl. under the meters) instead of the chart's column —
  // long (localized) labels then have room. State (not a ref) so the portal target triggers a
  // render once mounted. Both the freq and time charts target it, keeping the chart-row height
  // identical across the domain swap.
  const [legendHost, setLegendHost] = useState<HTMLDivElement | null>(null);
  // Undo/redo (v1): a single stack of whole-`stages` snapshots for the *current* slot, reset on
  // slot switch (deliberately not per-slot — a history that changes meaning when you switch slots
  // is more confusing than useful). Snapshots are the immutable `stages` object, so they cost
  // nothing to keep. `historyBaseline` holds the pre-gesture state so a whole drag/scrub becomes
  // one entry; `commitToken` bumps at each commit boundary to trigger the seal effect below.
  const [undoStack, setUndoStack] = useState<Stages[]>([]);
  const [redoStack, setRedoStack] = useState<Stages[]>([]);
  const [commitToken, setCommitToken] = useState(0);
  const historyBaseline = useRef<Stages | null>(null);
  const [showAllStages, setShowAllStages] = useState(false); // library filter: templates of all stages vs the active one
  // The chart area shows one of four mutually-exclusive views (a single segmented control):
  //   freq    — magnitude curves + spectrum backdrop (the default, editable)
  //   time    — impulse-response decay (§5.2)
  //   monitor — clean spectrum: curves stripped, spectrum + preamp only
  //   scope   — stereo vectorscope (X-Y goniometer) of the live loopback
  const [chartView, setChartView] = useState<"freq" | "time" | "monitor" | "scope">("freq");
  const impulseView = chartView === "time";
  const monitorView = chartView === "monitor";
  const scopeView = chartView === "scope";
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
  // Inline rename of a saved preset/template row (id + edited name). Committed on Enter/blur.
  const [renaming, setRenaming] = useState<{ kind: "preset" | "template"; id: string; name: string } | null>(null);
  const [expandedPreset, setExpandedPreset] = useState<string | null>(null); // which preset's version history is open
  const [presetSave, setPresetSave] = useState<UserPreset | null>(null); // the preset whose save dialog is open
  const [confirmBox, setConfirmBox] = useState<{ message: string; confirmLabel: string; onConfirm: () => void } | null>(null);
  const [selfTest, setSelfTest] = useState<SelfTestState | null>(null); // §5.4 output self-test
  // §5.2 solo: hear only one band within its stage (transient — never edits stored bands). A ref
  // mirror lets writeFit read the live value, and it auto-clears on any edit / slot / stage change.
  const [solo, setSolo] = useState<Solo | null>(null);
  const soloRef = useRef<Solo | null>(null);
  soloRef.current = solo;
  // §5.2 isolate: audition one (peaking) band's region — a bandpass, everything else off. A separate
  // apply path (the `isolate` command writes a bandpass-only config); mutually exclusive with solo.
  const [isolate, setIsolate] = useState<Solo | null>(null);
  const isolateRef = useRef<Solo | null>(null);
  isolateRef.current = isolate;
  // Finding #1: active directives in config.txt outside CAGEq's block that stack on top of every
  // correction (e.g. EqAPO's fresh-install default preamp/example filters). Detected once after
  // load; surfaced passively (never a first-run modal) as a line + reversible review panel.
  const [foreignConfig, setForeignConfig] = useState<string[] | null>(null);
  const [foreignReview, setForeignReview] = useState<null | "review" | "done">(null);
  const [foreignDismissed, setForeignDismissed] = useState(false);
  // App version (from tauri.conf.json via getVersion) — shown in the footer so a deployed build is
  // identifiable ("I'm on vX"). The single authoritative product version.
  const [appVersion, setAppVersion] = useState("");
  useEffect(() => {
    getVersion().then(setAppVersion).catch(() => {});
  }, []);
  const [activeSlot, setActiveSlot] = useState<SlotName>("A");
  // Drop a stale cross-view highlight when the underlying band list changes out from under a
  // still pointer (stage tab switch, slot change) — no pointerleave fires in that case.
  useEffect(() => setHoverBand(null), [activeStage, activeSlot]);
  // Last-applied inputs per editable slot (for display + reloading the controls).
  const [slotInputs, setSlotInputs] = useState<Record<"A" | "B", SlotInputs | null>>({ A: null, B: null });
  // Which preset (or archived version) was last loaded into each editable slot, with the
  // document sig captured at load time — so each slot can show its loaded preset name and a
  // "dirty" marker once the editor diverges from it. Persisted in the resume blob (below), so
  // the attribution — and any unsaved-edit dirty state — survives a restart. `ver` is the tag.
  const [slotPreset, setSlotPreset] = useState<Record<"A" | "B", LoadedRef | null>>({ A: null, B: null });
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
    // §5.2 solo: while a band is soloed, the *applied* cascade is the soloed view (auto-gain via the
    // §4.1 match), but we still store the real `inp` — solo is a monitoring state, not an edit.
    const soloing = soloRef.current != null;
    const applied = await invoke<ApplyResult>("apply", {
      device: dev.eqapo_pattern, // the EqAPO-matchable device pattern, not the headphone
      headphone: inp.measurementPath,
      target: inp.targetPath || null,
      slot,
      // Every enabled stage's enabled bands, summed into one list (§3.4). Bypassed bands and
      // disabled stages stay in the UI but are excluded from what's written.
      customFilters: appliedBands(soloStages(inp.stages, soloRef.current)),
    });
    setSlotInputs((prev) => ({ ...prev, [slot]: inp }));
    setHydrated((prev) => ({ ...prev, [slot]: true }));
    // Remember the composed fit so the next launch can restore this slot from cache (§3.5) — but not
    // the soloed cascade; keep the real cached fit so a solo can't leak into the persisted slot.
    if (!soloing)
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
        // Last session's output isn't in the active list (e.g. unplugged) but others are, so we
        // silently fell back to `useDev` — flag it so the user knows their device is missing.
        if (resume?.deviceId && !rdev && useDev) setResumeDeviceMissing(true);
        if (useDev) await invoke("set_device", { device: useDev.eqapo_pattern });

        const activeSlotName = resume?.activeSlot ?? "A";
        const activeInp = resume && resume.activeSlot !== "Dry" ? resume.slots?.[resume.activeSlot] : null;
        if (resume?.slots) {
          setSlotInputs({ A: resume.slots.A ?? null, B: resume.slots.B ?? null });
          setActiveSlot(activeSlotName);
        }
        if (resume?.slotPreset) setSlotPreset({ A: resume.slotPreset.A ?? null, B: resume.slotPreset.B ?? null });
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
          // The plain "Harman over-ear 2018" — matched exactly. (A `$`-anchored regex also matched
          // lab-prefixed variants like "crinacle … Harman over-ear 2018", which sort first and won.)
          const harman =
            tg.targets.find((t) => t.name.toLowerCase() === "harman over-ear 2018") ??
            tg.targets.find((t) => /^harman over-ear 2018\b/i.test(t.name));
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
        slotPreset: { A: slotPreset.A, B: slotPreset.B }, // which preset each slot was loaded from
      };
      invoke("set_resume", { resume }).catch(() => {});
    }, 400);
    return () => window.clearTimeout(id);
  }, [restored, activeSlot, deviceId, slotInputs, slotFits, activeStage, slotPreset]);

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

  // The pre-gain field is a ScrubNumber (like the band fields), so ↑/↓ are handled in JS on a
  // read-only text input — no native spinner racing the controlled value. It updates the local
  // value at once (responsive) and commits the backend on a **coalescing** throttle that mirrors
  // the tone `requestApply`: at most one set_loudness in flight, latest value wins, and the
  // response updates only `result` (the chart), never the field — so a held arrow can't be
  // clobbered. `loudnessRef` feeds the apply the latest settings without a stale closure.
  const loudnessRef = useRef(loudness);
  loudnessRef.current = loudness;
  // Commit a settled base-pre-gain change (PreampField owns the live editing + debounce, so this only
  // fires when the user settles): update App state once and apply. The response updates the chart
  // (`result`), never the field.
  function commitBasePregain(db: number) {
    const base = loudnessRef.current;
    if (!base) return;
    const next: LoudnessSettings = { ...base, base_pregain_db: Math.max(-40, Math.min(0, Math.round(db))) };
    setLoudness(next);
    loudnessRef.current = next;
    invoke<LoudnessUpdate>("set_loudness", { settings: next })
      .then((update) => {
        setError("");
        if (update.applied) setResult(update.applied);
      })
      .catch((e) => setError(String(e)));
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
      if (!auto) setError(tr("errors.pickHeadphone"));
      return;
    }
    const dev = devices.find((d) => d.id === deviceId);
    if (!dev) {
      if (!auto) setError(tr("errors.pickDevice"));
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

  // §5.2 isolate write: a bandpass-only config at the band's current Fc/Q. In-flight-guarded like
  // apply() so it shares the coalescing throttle below — the backend reads the isolated device from
  // the slot cache, so no headphone/target args are needed.
  async function applyIsolate(freqHz: number, q: number) {
    try {
      inFlight.current = true;
      setResult(await invoke<ApplyResult>("isolate", { freqHz, q }));
    } catch (e) {
      setError(String(e));
    } finally {
      inFlight.current = false;
    }
  }

  // --- live tone editing -----------------------------------------------------
  // Tone changes auto-apply so they're audible immediately. A coalescing throttle
  // keeps a drag streaming updates (rather than only firing when you let go) without
  // flooding IPC; the Core separately enforces the §5.3 >=15 ms write spacing.
  const inFlight = useRef(false);
  const applyTimer = useRef<number | null>(null);
  const autoApplyRef = useRef<() => void>(() => {});

  // Refreshed every render so a queued timer never fires against stale state. While a band is
  // isolated the queued write is a bandpass re-write at its *latest* Fc/Q (so a drag sweeps it),
  // routed through the same throttle as a normal apply — one write in flight, drag frames fold.
  autoApplyRef.current = () => {
    if (inFlight.current) {
      requestApply(60); // a write is in progress — retry shortly
      return;
    }
    const iso = isolateRef.current;
    if (iso) {
      const band = stages[iso.stage]?.bands[iso.idx];
      if (band) {
        void applyIsolate(band.freq_hz, band.q);
        return;
      }
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

  // §5.2 solo — toggle "hear only this band" for the active stage. Setting `solo` re-applies the
  // soloed cascade; toggling off re-applies the normal one. Only on A/B (Dry isn't editable).
  const toggleSolo = (idx: number) => {
    if (dryActive) return;
    isolateRef.current = null;
    setIsolate(null); // solo and isolate are mutually exclusive audition modes
    const cur = soloRef.current;
    const next = cur && cur.stage === activeStage && cur.idx === idx ? null : { stage: activeStage, idx };
    soloRef.current = next; // writeFit reads this immediately; the delayed apply picks it up
    setSolo(next);
    requestApply(0);
  };
  // §5.2 isolate — toggle a bandpass audition of one band's region (peaking bands only; a bandpass
  // is meaningless for a shelf). Writes a bandpass-only config via the `isolate` command; toggling
  // off (or any auto-clear) re-applies the normal cascade. Mutually exclusive with solo.
  const toggleIsolate = (idx: number) => {
    if (dryActive) return;
    const band = stages[activeStage].bands[idx];
    if (!band || band.kind !== "Peaking") return;
    const cur = isolateRef.current;
    if (cur && cur.stage === activeStage && cur.idx === idx) {
      clearIsolate();
      return;
    }
    soloRef.current = null;
    setSolo(null); // mutual exclusion
    isolateRef.current = { stage: activeStage, idx };
    setIsolate({ stage: activeStage, idx });
    setError("");
    requestApply(0); // autoApplyRef sees isolateRef → writes the bandpass (throttled, coalesced)
  };
  // Auto-clear the transient audition modes and restore the normal cascade. No-ops when inactive;
  // the re-apply folds into an edit's own apply when one is already queued.
  const clearSolo = () => {
    if (soloRef.current == null) return;
    soloRef.current = null;
    setSolo(null);
    requestApply(0);
  };
  const clearIsolate = () => {
    if (isolateRef.current == null) return;
    isolateRef.current = null;
    setIsolate(null);
    requestApply(0); // overwrite the bandpass config with the real cascade
  };
  // Exit any audition *without* re-applying — for structural edits (add/remove/load/undo/toggleStage)
  // that already fire their own apply; nulling the refs first makes that apply write the normal
  // cascade. The reset is driven imperatively from each handler rather than a `stages`-watching
  // effect, so editing the audition band itself (which also changes `stages`) can't trip it.
  const dropAudition = () => {
    if (soloRef.current) {
      soloRef.current = null;
      setSolo(null);
    }
    if (isolateRef.current) {
      isolateRef.current = null;
      setIsolate(null);
    }
  };
  // A stage-tab switch exits the audition (the band isn't visible in another stage) and restores
  // the normal cascade — the one auto-exit that isn't already covered by an edit's own apply.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(() => {
    clearSolo();
    clearIsolate();
  }, [activeStage]);

  // --- undo/redo history --------------------------------------------------------------------
  // Remember the state *before* the current gesture (the first edit since the last commit); a
  // no-op if a gesture is already in progress, so a 200-frame drag captures one baseline.
  const captureBaseline = () => {
    if (historyBaseline.current === null) historyBaseline.current = stages;
  };
  // Mark a commit boundary (gesture end / discrete action). The seal effect turns the captured
  // baseline into an undo entry — done in an effect, not here, so it reads the settled `stages`.
  const commitHistory = () => setCommitToken((t) => t + 1);
  const resetHistory = () => {
    setUndoStack([]);
    setRedoStack([]);
    historyBaseline.current = null;
  };
  // Seal a completed gesture into the undo stack. Keyed on commitToken (NOT stages) so it fires
  // only at commit boundaries, never on the throttled inputs mid-drag; `stages` here is the
  // settled post-gesture value. Skips no-op commits (baseline === result) so undo always does
  // something visible. Clears redo — a fresh edit forks the timeline.
  useEffect(() => {
    if (commitToken === 0) return;
    const base = historyBaseline.current;
    historyBaseline.current = null;
    if (base && JSON.stringify(base) !== JSON.stringify(stages)) {
      setUndoStack((u) => [...u, base].slice(-50));
      setRedoStack([]);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [commitToken]);
  // Switching the editable slot swaps the whole document — start its history fresh (covers both
  // switchSlot and copySlot, which both change activeSlot). Runs on mount too (harmless: empty).
  useEffect(() => {
    resetHistory();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeSlot]);

  const undo = () => {
    if (activeSlot === "Dry" || undoStack.length === 0) return;
    const prev = undoStack[undoStack.length - 1];
    historyBaseline.current = null; // the restore itself must not seal a new entry
    dropAudition();
    setRedoStack((r) => [...r, stages].slice(-50));
    setUndoStack((u) => u.slice(0, -1));
    setStages(prev);
    requestApply(0);
  };
  const redo = () => {
    if (activeSlot === "Dry" || redoStack.length === 0) return;
    const next = redoStack[redoStack.length - 1];
    historyBaseline.current = null;
    dropAudition();
    setUndoStack((u) => [...u, stages].slice(-50));
    setRedoStack((r) => r.slice(0, -1));
    setStages(next);
    requestApply(0);
  };

  // Switch the active comparison slot. A hydrated A/B (fit cached this session) and Dry
  // write instantly (pure re-write, no re-fit); a slot whose inputs were restored from a
  // previous session but not yet fitted this session is re-fitted on first switch; an
  // empty A/B just becomes the editable target.
  async function switchSlot(slot: SlotName) {
    if (slot === activeSlot) return;
    // Leaving a soloed slot: restore its *normal* cascade first (the backend slot cache is soloed),
    // so switching back later re-writes the real correction, not the solo.
    if (soloRef.current) {
      soloRef.current = null;
      setSolo(null);
      await apply(true);
    }
    // Isolate writes directly (doesn't touch the slot cache), so just drop the state — activating
    // the target slot below overwrites the bandpass config with that slot's real correction.
    if (isolateRef.current) {
      isolateRef.current = null;
      setIsolate(null);
    }
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
      setError(tr("errors.slotEmpty", { from }));
      return;
    }
    try {
      setError("");
      dropAudition(); // switching to the copy replaces the document — exit any solo/isolate
      const applied = await invoke<ApplyResult>("copy_slot", { from, to });
      setSlotInputs((prev) => ({ ...prev, [to]: src }));
      setSlotPreset((prev) => ({ ...prev, [to]: prev[from] })); // carry the source's preset attribution
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
    setResumeDeviceMissing(false); // the user has now made an explicit choice
    const dev = devices.find((d) => d.id === id);
    if (dev) await invoke("set_device", { device: dev.eqapo_pattern });
  }

  // Detach the vectorscope into its own resizable window (index.html#scope → ScopeWindow). The
  // loopback monitor keeps running here; the scope events are app-global, so the new window just
  // listens. Focuses the existing one instead of spawning a duplicate.
  async function openScopeWindow() {
    const existing = await WebviewWindow.getByLabel("scope");
    if (existing) {
      await existing.setFocus();
      return;
    }
    const w = new WebviewWindow("scope", {
      url: "index.html#scope",
      title: "CAGEq — Vectorscope",
      width: 760,
      height: 800,
      minWidth: 320,
      minHeight: 320,
      resizable: true,
    });
    // Count the pop-out as a scope viewer for its whole lifetime (so the backend emits the scope
    // stream) and release it on close — tracked here, in the always-alive main window, because the
    // pop-out's own React cleanup may not run when its OS window is destroyed. Counted optimistically
    // (the window object exists); released on destroy or a creation error, once.
    void invoke("set_scope_viewer", { active: true });
    let counted = true;
    const release = () => {
      if (counted) {
        counted = false;
        void invoke("set_scope_viewer", { active: false });
      }
    };
    w.once("tauri://destroyed", release);
    w.once("tauri://error", (e) => {
      setError(String(e.payload));
      release();
    });
  }

  // Finding #1: detect foreign config.txt directives once the backend is up (read-only).
  useEffect(() => {
    if (loading) return;
    invoke<string[]>("config_foreign_directives").then(setForeignConfig).catch(() => {});
  }, [loading]);

  // Comment out the foreign directives (reversible) so only CAGEq's correction applies.
  async function commentOutForeign() {
    try {
      setError("");
      await invoke("disable_foreign_config");
      setForeignConfig([]); // now none active
      setForeignReview("done");
    } catch (e) {
      setError(String(e));
    }
  }
  // Undo the comment-out, restoring the original directives.
  async function restoreForeign() {
    try {
      setError("");
      await invoke("restore_foreign_config");
      setForeignConfig(await invoke<string[]>("config_foreign_directives"));
      setForeignReview(null);
    } catch (e) {
      setError(String(e));
    }
  }

  // §8: jump to the modern Windows Sound settings for the selected output so the user can change
  // its playback format there (CAGEq only reads the sample rate, never sets the format). The
  // backend picks the right deep-link (default device → its properties page, else the device list).
  const openOutputSettings = () => {
    void invoke("open_output_settings", { device: deviceId || null }).catch((e) => setError(String(e)));
  };
  // Windows playback rate, shown compactly (48 kHz, 44.1 kHz, 96 kHz…).
  const fmtRate = (hz: number) => `${+(hz / 1000).toFixed(1)} kHz`;
  // Compact local ISO timestamp for a saved preset version (YYYY-MM-DD HH:MM — unambiguous across
  // locales and aligns cleanly in the list); legacy/migrated versions have at=0 → "—".
  const fmtWhen = (at: number) => {
    if (at <= 0) return "—";
    const d = new Date(at);
    const p = (n: number) => String(n).padStart(2, "0");
    return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
  };

  // §5.4 output self-test: play pink noise (through EqAPO) and check the loopback spectrum's shape
  // matches the applied correction — proving the EQ actually reaches the output. Runs entirely off
  // the current correction (no config change); the backend only plays the signal.
  async function runSelfTest() {
    if (!result || dryActive || (activeSlot !== "A" && activeSlot !== "B")) return;
    const bands = result.filters;
    const originalSlot = activeSlot; // restored after the Dry reference capture
    // Pre-check: if the correction is too flat there's no shape to measure — don't bother playing.
    const grid = new Float64Array(120);
    for (let i = 0; i < 120; i++) grid[i] = 20 * 1000 ** (i / 119); // 20 Hz … 20 kHz
    const gc = composedCurveDb(bands, grid, sampleRate ?? undefined);
    let mean = 0;
    for (const v of gc) mean += v;
    mean /= gc.length;
    let varSum = 0;
    for (const v of gc) varSum += (v - mean) ** 2;
    if (Math.sqrt(varSum / gc.length) < 1.5) {
      setSelfTest({ phase: "done", verdict: { kind: "inconclusive" } });
      return;
    }
    // Capture the loopback spectrum for `ms`, after `settleMs` for the config change to take hold.
    const capture = async (settleMs: number, ms: number): Promise<SpectrumData[]> => {
      await sleep(settleMs);
      const frames: SpectrumData[] = [];
      const un = await listen<SpectrumData>("spectrum", (e) => frames.push(e.payload));
      try {
        await sleep(ms);
      } finally {
        un(); // always detach — an exception here must not leave this pushing into `frames` forever
      }
      return frames;
    };
    setSelfTest({ phase: "running" });
    let switched = false; // true while the applied config is temporarily forced to Dry
    try {
      setError("");
      await invoke("start_test_signal", { device: deviceId || null });
      // 1) measure the current correction, 2) bypass to Dry and measure the reference, 3) restore.
      const corrected = await capture(500, 1500); // fade-in + settle, then capture
      if (corrected.length < 3) {
        // Loopback produced nothing (device disabled/removed) — don't bother toggling Dry.
        setSelfTest({ phase: "done", verdict: { kind: "nosignal" } });
        return;
      }
      await invoke("activate_slot", { slot: "Dry" });
      switched = true;
      const dry = await capture(600, 1500); // EqAPO reload/crossfade + spectrum smoothing settle
      await invoke("activate_slot", { slot: originalSlot });
      switched = false;
      setSelfTest({ phase: "done", verdict: selfTestVerdict(corrected, dry, bands, sampleRate ?? undefined) });
    } catch (e) {
      setError(String(e));
      setSelfTest(null);
    } finally {
      if (switched) await invoke("activate_slot", { slot: originalSlot }).catch(() => {}); // never leave Dry applied
      await invoke("stop_test_signal").catch(() => {});
    }
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
    captureBaseline();
    dropAudition(); // a new band is a structural change — exit any solo/isolate
    const idx = stages[activeStage].bands.length;
    setStageBands(activeStage, (cf) => [...cf, { kind: "Peaking", freq_hz, gain_db, q: 1 }]);
    markNewBand(idx, false);
    requestApply(0);
    commitHistory();
  };
  // The Add button (keyboard/discoverability path): drop a flat band into the widest empty
  // gap between existing bands (in log-frequency), so it never lands on top of a neighbour,
  // and open its Fc for typing straight away (keyboard-first).
  const addFilter = () => {
    captureBaseline();
    dropAudition(); // a new band is a structural change — exit any solo/isolate
    const bands = stages[activeStage].bands;
    const idx = bands.length;
    setStageBands(activeStage, (cf) => [...cf, { kind: "Peaking", freq_hz: widestGapHz(bands), gain_db: 0, q: 1 }]);
    markNewBand(idx, true);
    requestApply(0);
    commitHistory();
  };
  const updateFilter = (i: number, patch: Partial<CustomFilter>, delay = 0) => {
    captureBaseline();
    const iso = isolateRef.current;
    const sol = soloRef.current;
    const editingIso = iso != null && iso.stage === activeStage && iso.idx === i;
    const editingSolo = sol != null && sol.stage === activeStage && sol.idx === i;
    setStageBands(activeStage, (cf) => cf.map((f, j) => (j === i ? { ...f, ...patch } : f)));
    if (editingIso || editingSolo) {
      // Editing the audition band itself keeps it alive: the throttled apply re-reads the isolated
      // band's latest Fc/Q (sweeps the bandpass) or re-applies the soloed cascade with the edit.
      requestApply(delay);
    } else {
      // Editing any *other* band exits the audition and returns to the normal cascade.
      if (sol || iso) dropAudition();
      requestApply(delay);
    }
    // delay 0 is the final/commit call (scrub release, blur, Enter, kind cycle, bypass toggle);
    // delay 70 is a live throttled input mid-drag, which must not seal a history entry.
    if (delay === 0) commitHistory();
  };
  const removeFilter = (i: number) => {
    // The fixed Bass/Treble/Air macros are never removable (the grid hides their ✕; the
    // chart's double-click-to-remove would otherwise slip past that). Guard at the source.
    if (stages[activeStage].bands[i]?.fixed) return;
    captureBaseline();
    dropAudition(); // removing a band shifts indices — exit any solo/isolate
    setStageBands(activeStage, (cf) => cf.filter((_, j) => j !== i));
    setNewBand(null); // storage indices shift on removal — drop any stale highlight
    requestApply(0);
    commitHistory();
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
    captureBaseline();
    dropAudition(); // enabling/disabling a stage changes the cascade — exit any solo/isolate
    setStages((st) => ({ ...st, [id]: { ...st[id], enabled: !st[id].enabled } }));
    requestApply(0);
    commitHistory();
  };
  // --- §3.5 preset library: load / save / delete -----------------------------
  // Loading always targets the *active* slot (disabled on Dry). A filter template drops its
  // bands into *its* stage (and switches to that tab); a full preset replaces measurement +
  // target + all stages.
  const loadTemplate = (t: FilterTemplate) => {
    if (activeSlot === "Dry") return;
    captureBaseline(); // dropping a template's bands into a stage is one undoable step
    dropAudition();
    const bands = t.stage === "tone" ? ensureMacros(t.bands) : t.bands;
    setStages((st) => ({ ...st, [t.stage]: { enabled: true, bands } }));
    setActiveStage(t.stage);
    requestApply(0);
    commitHistory();
  };
  // Loads a preset *or* one of its archived versions (both are PresetState) into the active slot.
  // `ref` carries the parent preset's identity + which version, so the slot can show what's
  // loaded and flag divergence (dirty). The sig is captured from the exact normalized state applied.
  const loadPreset = (p: PresetState, ref?: { id: string; name: string; at?: number | "head" }) => {
    if (activeSlot === "Dry") return;
    dropAudition();
    const stages = normalizeStages(p.stages);
    setQuery(p.model);
    setMeasurementPath(p.measurementPath);
    setTargetPath(p.targetPath);
    setStages(stages);
    requestApply(0);
    // A full preset swaps measurement + target + all stages — a new document, not an edit; undo
    // can't half-restore those (they're outside history), so start fresh rather than mislead.
    resetHistory();
    if (activeSlot === "A" || activeSlot === "B")
      setSlotPreset((sp) => ({
        ...sp,
        [activeSlot]: ref ? { ...ref, sig: presetSig(p.model, p.measurementPath, p.targetPath, stages) } : null,
      }));
  };
  // Re-anchor the active slot's loaded-preset attribution to a freshly saved state (so a
  // save/overwrite clears the dirty flag and updates the vN tag). Uses the current editor
  // state as the new clean baseline. Always the preset's head — a save/overwrite/version-save
  // always writes (or promotes the editor state to) the head.
  const setActiveSlotLoaded = (id: string, name: string) => {
    if (activeSlot !== "A" && activeSlot !== "B") return;
    setSlotPreset((sp) => ({ ...sp, [activeSlot]: { id, name, at: "head", sig: presetSig(query, measurementPath, targetPath, stages) } }));
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
    // Overwriting an existing preset by name keeps its version history.
    const existingVersions = kind === "preset" ? (existing as UserPreset | undefined)?.versions : undefined;
    const write = () => {
      setLibrary((lib) =>
        kind === "preset"
          ? { ...lib, presets: upsert(lib.presets, { id, name, model: query, measurementPath, targetPath, stages, versions: existingVersions }) }
          : { ...lib, templates: upsert(lib.templates, { id, name, stage: activeStage, bands: stages[activeStage].bands }) },
      );
      if (kind === "preset") setActiveSlotLoaded(id, name);
      setSaveForm(null);
    };
    if (existing) {
      setConfirmBox({
        message:
          kind === "preset"
            ? tr("dialog.overwritePreset", { name })
            : tr("dialog.overwriteTemplate", { stage: stageLabel(activeStage), name }),
        confirmLabel: tr("dialog.overwrite"),
        onConfirm: write,
      });
    } else {
      write();
    }
  }

  // Overwrite an existing saved entry in place with the *current* controls (same id +
  // name) — the per-row "update" button, so tweaking a loaded preset and saving it back
  // doesn't mean retyping the name. Confirms first (reusing the overwrite dialog).
  // Overwrite the preset's current state in place with the editor state (no new history entry) —
  // the "Overwrite current" choice of the save dialog. Preserves the version history.
  const overwritePresetInPlace = (p: UserPreset) => {
    setLibrary((lib) => ({
      ...lib,
      presets: upsert(lib.presets, { id: p.id, name: p.name, model: query, measurementPath, targetPath, stages, versions: p.versions }),
    }));
    setActiveSlotLoaded(p.id, p.name);
  };

  // §3.5 preset versioning — explicit only. "Save version" archives the preset's *current stored*
  // state into its history and advances the preset to the current editor state, so refining →
  // save-version keeps the prior state to A/B against. Capped; nothing is captured automatically.
  const MAX_VERSIONS = 10;
  const saveNewVersion = (p: UserPreset) => {
    setLibrary((lib) => ({
      ...lib,
      presets: upsert(lib.presets, {
        id: p.id,
        name: p.name,
        model: query,
        measurementPath,
        targetPath,
        stages,
        versions: [
          ...(p.versions ?? []),
          { at: Date.now(), model: p.model, measurementPath: p.measurementPath, targetPath: p.targetPath, stages: p.stages },
        ].slice(-MAX_VERSIONS),
      }),
    }));
    // The prior stored head becomes a version and the editor state becomes the new head, clean.
    setActiveSlotLoaded(p.id, p.name);
  };
  // Delete asks first — same confirm overlay as the other destructive actions (deletePreset et al).
  const deleteVersion = (p: UserPreset, idx: number) =>
    setConfirmBox({
      message: tr("dialog.deleteVersion", { name: p.name, ver: `v${idx + 1}` }),
      confirmLabel: tr("dialog.delete"),
      onConfirm: () =>
        setLibrary((lib) => ({
          ...lib,
          presets: lib.presets.map((x) => (x.id === p.id ? { ...x, versions: (x.versions ?? []).filter((_, i) => i !== idx) } : x)),
        })),
    });
  // Delete the preset's *newest* version — the head — reverting it to the most recent archived
  // version, which becomes the new head. The counterpart to "save version" (which pushes the old
  // head onto the stack); this pops it back off. Only offered when there's a prior version to
  // fall back to — with none, "deleting the newest version" is deleting the whole preset, which
  // the row's own trash button already does.
  const deleteHeadVersion = (p: UserPreset) => {
    const versions = p.versions ?? [];
    if (versions.length === 0) return;
    const prev = versions[versions.length - 1];
    setConfirmBox({
      message: tr("dialog.deleteHeadVersion", { name: p.name, ver: `v${versions.length + 1}`, prevVer: `v${versions.length}` }),
      confirmLabel: tr("dialog.delete"),
      onConfirm: () =>
        setLibrary((lib) => ({
          ...lib,
          presets: lib.presets.map((x) =>
            x.id === p.id
              ? { id: x.id, name: x.name, model: prev.model, measurementPath: prev.measurementPath, targetPath: prev.targetPath, stages: prev.stages, versions: versions.slice(0, -1) }
              : x,
          ),
        })),
    });
  };
  // A template row's "update" saves the current bands of *that template's* stage.
  const updateTemplate = (t: FilterTemplate) =>
    setConfirmBox({
      message: tr("dialog.updateTemplate", { stage: stageLabel(t.stage), name: t.name }),
      confirmLabel: tr("dialog.overwrite"),
      onConfirm: () =>
        setLibrary((lib) => ({ ...lib, templates: upsert(lib.templates, { id: t.id, name: t.name, stage: t.stage, bands: stages[t.stage].bands }) })),
    });

  // Delete asks first — same confirm overlay as the other destructive actions.
  const deletePreset = (p: UserPreset) =>
    setConfirmBox({
      message: tr("dialog.deletePreset", { name: p.name }),
      confirmLabel: tr("dialog.delete"),
      onConfirm: () => setLibrary((lib) => ({ ...lib, presets: lib.presets.filter((x) => x.id !== p.id) })),
    });
  const deleteTemplate = (t: FilterTemplate) =>
    setConfirmBox({
      message: tr("dialog.deleteTemplate", { name: t.name }),
      confirmLabel: tr("dialog.delete"),
      onConfirm: () => setLibrary((lib) => ({ ...lib, templates: lib.templates.filter((x) => x.id !== t.id) })),
    });

  // Inline rename: write the trimmed name back to the matching entry (empty name = cancel),
  // preserving the id so the row stays put and its persisted identity is unchanged.
  const commitRename = () => {
    if (!renaming) return;
    const name = renaming.name.trim();
    const { kind, id } = renaming;
    if (name)
      setLibrary((lib) =>
        kind === "preset"
          ? { ...lib, presets: lib.presets.map((x) => (x.id === id ? { ...x, name } : x)) }
          : { ...lib, templates: lib.templates.map((x) => (x.id === id ? { ...x, name } : x)) },
      );
    setRenaming(null);
  };

  // A/S/D switch slots, W toggles loudness mode — but not while typing in a field
  // (§5.2 blind-comparison shortcuts).
  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      const el = document.activeElement as HTMLElement | null;
      // Undo/redo first, and only blocked by a genuine *text-editing* field (so its native
      // text-undo wins). A range fader keeps focus after a drag but isn't text — undo must still
      // work there (that was the "Ctrl+Z does nothing after a slider drag" bug).
      const mod = e.ctrlKey || e.metaKey;
      const k = e.key.toLowerCase();
      if (mod && (k === "z" || k === "y")) {
        const type = el?.tagName === "INPUT" ? ((el as HTMLInputElement).type || "text").toLowerCase() : "";
        const inTextField =
          !!el &&
          (el.tagName === "TEXTAREA" ||
            el.isContentEditable ||
            (el.tagName === "INPUT" && !["range", "checkbox", "radio", "button", "submit", "reset", "file", "color"].includes(type)));
        if (inTextField) return;
        e.preventDefault();
        if (k === "y" || e.shiftKey) redo();
        else undo();
        return;
      }
      // Plain-letter slot/loudness shortcuts: suppressed whenever any form field has focus.
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

  // Broadcast the active EQ cascade to any scope view (inline or the detached window) so its
  // inverse-filter ("undistort") mode can recover the pre-EQ image. Dry → no filters (already
  // pristine). Emitted on change, and replayed on request when a scope window mounts and asks.
  // Dry has no EQ to invert, but it DOES carry a preamp (the §4.1 loudness match in Comparison
  // mode), so undistort must undo it too — else Dry's scope excursion wouldn't match A/B's.
  const scopeEq = { filters: dryActive ? [] : result?.filters ?? [], preampDb: result?.preamp_db ?? 0 };
  const scopeEqRef = useRef(scopeEq);
  scopeEqRef.current = scopeEq;
  useEffect(() => {
    void emit("scope-eq", scopeEqRef.current);
  }, [result, dryActive]);
  useEffect(() => {
    let un: (() => void) | undefined;
    void (async () => {
      un = await listen("scope-eq-request", () => void emit("scope-eq", scopeEqRef.current));
    })();
    return () => un?.();
  }, []);

  // Has a slot's document diverged from the preset last loaded into it? For the active slot the
  // live editor is the source of truth; an inactive slot compares its last-applied snapshot.
  const slotDirty = (s: "A" | "B") => {
    const ref = slotPreset[s];
    if (!ref) return false;
    if (s === activeSlot) return presetSig(query, measurementPath, targetPath, stages) !== ref.sig;
    const inp = slotInputs[s];
    return !inp || presetSig(inp.model, inp.measurementPath, inp.targetPath, inp.stages) !== ref.sig;
  };
  // The vN label for a loaded ref, derived live from the current library instead of a cached
  // string — position-based labels (v1, v2, …) shift whenever *any* version of that preset is
  // added or deleted, so a value captured at load time goes stale the moment that happens. Null
  // when the preset (or, for an archived load, that specific version) no longer exists.
  const presetVerLabel = (ref: LoadedRef): string | null => {
    const p = library.presets.find((x) => x.id === ref.id);
    if (!p) return null;
    const versions = p.versions ?? [];
    if (ref.at === "head") return `v${versions.length + 1}`;
    if (ref.at == null) return null; // legacy resume blob from before `at` existed
    const idx = versions.findIndex((v) => v.at === ref.at);
    return idx === -1 ? null : `v${idx + 1}`;
  };

  // Measure the chart's rendered SVG so the meter bars can match its plot-area Y extent. EqChart's
  // viewBox is 720×215 with PAD.t=12 / PAD.b=24 → the plot spans y 12..191 of 215.
  useEffect(() => {
    const svg = chartWrapRef.current?.querySelector("svg");
    // The scope view is a canvas (no SVG); keep the last measured box so the meters beside it don't
    // jump. When there's genuinely no chart (result cleared) the whole row unmounts anyway.
    if (!svg) return;
    const measure = () => {
      const h = svg.getBoundingClientRect().height;
      setPlotBox(h > 0 ? { top: (12 / 215) * h, height: (179 / 215) * h } : null);
    };
    const ro = new ResizeObserver(measure);
    ro.observe(svg);
    measure();
    return () => ro.disconnect();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [chartView, dryActive, loading, !!result]);

  // The active stage's bands (the grid/nodes edit these) and the full applied custom set.
  const activeBands = stages[activeStage].bands;
  const appliedCustom = useMemo(() => appliedBands(stages), [stages]);
  // Library filtering: templates for the active stage unless "show all" is on; curated
  // shapes are all Tone-staged, so they only show on the Tone tab (or with show-all).
  const visibleTemplates = showAllStages ? library.templates : library.templates.filter((t) => t.stage === activeStage);
  const showCurated = showAllStages || activeStage === "tone";

  // The active slot's cached composed fit. Slot-synced: it indexes straight off `activeSlot`, so
  // it's already correct in the same paint as a slot switch — unlike the live `result`, which
  // arrives a beat later once activate_slot / the re-fit resolves.
  const activeFit = activeSlot === "A" || activeSlot === "B" ? slotFits[activeSlot] : null;

  // The active slot's composed bands split into its AutoEq fit and the custom tail. The sidecar
  // appends the (summed) custom filters after the AutoEq bands, so the fit is everything before
  // that tail. Drawn as fixed diamonds; the tone bands stay draggable. Sourced from the cached
  // fit first so the read-only AutoEq stage doesn't flash a wrong-length slice of the *previous*
  // slot's `result` during a switch (result lags `stages`/`appliedCustom` by a beat); falls back
  // to the live `result` only for a slot with no cached fit yet.
  const autoEqSource = activeFit?.filters ?? (result && !dryActive ? result.filters : null);
  const autoEqBands = useMemo(() => {
    if (dryActive || !autoEqSource) return [];
    const n = Math.max(0, autoEqSource.length - appliedCustom.length);
    return autoEqSource.slice(0, n);
  }, [autoEqSource, appliedCustom, dryActive]);
  // Drop the read-only AutoEq view when there's nothing to show (Dry, or no fit yet) so its tab
  // never lingers active over an empty grid.
  useEffect(() => {
    if (autoEqBands.length === 0) setAutoEqView(false);
  }, [autoEqBands.length]);
  // Tab count comes straight from the same slot-synced source, so it renders in the same paint.
  const autoEqCount = autoEqBands.length;

  // In Comparison mode, hold the chart's Y-scale steady across A/B switches by flooring it at the
  // larger of both editable slots' curve ranges — otherwise switching to the flatter slot rescales
  // the graph, which is jarring when A/B-ing. Only in Comparison (where A/B are meant to be read
  // against each other); Final volume auto-ranges per slot. It's a floor, so a live edit still grows
  // the scale as needed.
  const comparisonSpan = useMemo(() => {
    if (loudness?.mode !== "Comparison") return undefined;
    const freqs = logGrid(240, 20, 20000);
    let m = 0;
    for (const s of ["A", "B"] as const) {
      const fit = slotFits[s];
      if (!fit) continue;
      for (const v of composedCurveDb(fit.filters, freqs)) m = Math.max(m, Math.abs(v));
    }
    return m > 0 ? Math.max(6, Math.ceil(m + 1)) : undefined;
  }, [loudness?.mode, slotFits]);

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
        label: slotLabel(activeSlot),
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
          out.push({ id: `stage-${id}`, bands, color: STAGE_COLOR[id], label: stageLabel(id), muted: id !== activeStage });
        }
      }
    }
    return out;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [result, activeSlot, dryActive, stages, activeStage, i18n.language]);

  const chartMarkers: Marker[] = useMemo(
    // Off by default — a nerd overlay revealed from the legend.
    () => (autoEqBands.length ? [{ id: "autoeq", bands: autoEqBands, color: SLOT_COLOR[activeSlot], label: tr("chart.autoeq"), defaultHidden: true }] : []),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [autoEqBands, activeSlot, i18n.language],
  );

  // The ideal correction the active slot's fit chases (§5.2): the AutoEq curve should
  // hug it; the gap is the residual the parametric fit couldn't capture. Off for Dry.
  const chartRefs: RefCurve[] = useMemo(() => {
    if (dryActive) return [];
    const out: RefCurve[] = [];
    if (result?.reference_curve?.length)
      out.push({ id: "ideal", points: result.reference_curve, color: REF_COLOR, label: tr("chart.idealEq") });
    // Target + raw measurement — nerd overlays, off by default (share the dBr reference).
    if (targetCurve?.length)
      out.push({ id: "target", points: targetCurve, color: TARGET_COLOR, label: tr("chart.target"), defaultHidden: true });
    if (rawCurve?.length)
      out.push({ id: "raw", points: rawCurve, color: RAW_COLOR, label: tr("chart.raw"), defaultHidden: true });
    return out;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [result, dryActive, rawCurve, targetCurve, i18n.language]);

  // Phase of the applied filter chain, on the secondary axis — a nerd overlay, off by default.
  const chartPhase: PhaseCurve | undefined = useMemo(
    () => (!dryActive && result ? { id: "phase", bands: result.filters, color: PHASE_COLOR, label: tr("chart.phase"), defaultHidden: true } : undefined),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [result, dryActive, i18n.language],
  );

  // Fail-safe / startup banner (§5.1). Watchdog states are live; the startup verdicts
  // only matter until the user applies something (they describe the state at launch).
  const banner = (() => {
    if (!status) return null;
    if (status.health_kind === "Terminal")
      return { critical: true, text: tr("banner.terminal"), retry: true };
    if (status.health_kind === "Recovering")
      return { critical: false, text: tr("banner.recovering"), retry: false };
    if (!result && status.startup === "SafeStateStillActive")
      return { critical: false, text: tr("banner.safeState"), retry: false };
    if (!result && status.startup === "ExternallyModified")
      return { critical: false, text: tr("banner.externallyModified"), retry: false };
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
              <Trans i18nKey="dialog.finalTitle" values={{ jump: pendingFinal.jump.toFixed(1) }} components={[<b />, <b />]} />
            </p>
            <label style={{ fontSize: "0.85em", display: "block", margin: "0.6em 0" }}>
              <input type="checkbox" checked={dontAskAgain} onChange={(e) => setDontAskAgain(e.currentTarget.checked)} />{" "}
              {tr("dialog.dontAskAgain")}
            </label>
            <div className="row" style={{ justifyContent: "flex-end", gap: "0.5em" }}>
              <button type="button" onClick={() => setPendingFinal(null)}>
                {tr("dialog.cancel")}
              </button>
              <button type="button" onClick={confirmFinal}>
                {tr("dialog.switchFinal")}
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
                {tr("dialog.cancel")}
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

      {presetSave && (
        <div
          onClick={() => setPresetSave(null)}
          style={{ position: "fixed", inset: 0, background: "#0006", display: "flex", alignItems: "center", justifyContent: "center", zIndex: 10 }}
        >
          <div className="modal-card" onClick={(e) => e.stopPropagation()}>
            <p style={{ marginTop: 0, fontWeight: 600 }}>{tr("dialog.savePresetTitle", { name: presetSave.name })}</p>
            <p style={{ fontSize: "0.85em", opacity: 0.75 }}>{tr("dialog.savePresetHint")}</p>
            {/* Stacked, full-width: three verbose choices (esp. in German) wrap badly side by side. */}
            <div style={{ display: "flex", flexDirection: "column", gap: "0.4em", marginTop: "0.8em" }}>
              <button
                type="button"
                onClick={() => {
                  saveNewVersion(presetSave);
                  setExpandedPreset(presetSave.id); // reveal the freshly-archived version
                  setPresetSave(null);
                }}
              >
                {tr("dialog.saveNewVersion")}
              </button>
              <button
                type="button"
                onClick={() => {
                  overwritePresetInPlace(presetSave);
                  setPresetSave(null);
                }}
              >
                {tr("dialog.overwriteCurrent")}
              </button>
              <button type="button" onClick={() => setPresetSave(null)}>
                {tr("dialog.cancel")}
              </button>
            </div>
          </div>
        </div>
      )}

      {selfTest && (
        <div
          onClick={() => selfTest.phase !== "running" && setSelfTest(null)}
          style={{ position: "fixed", inset: 0, background: "#0006", display: "flex", alignItems: "center", justifyContent: "center", zIndex: 10 }}
        >
          <div className="modal-card" onClick={(e) => e.stopPropagation()}>
            {selfTest.phase === "warn" && (
              <>
                <p style={{ marginTop: 0, fontWeight: 600 }}>{tr("selfTest.warnTitle")}</p>
                <p style={{ fontSize: "0.9em" }}>{tr("selfTest.warnBody", { device: selectedDevice?.name ?? "" })}</p>
                <div className="row" style={{ justifyContent: "flex-end", gap: "0.5em" }}>
                  <button type="button" onClick={() => setSelfTest(null)}>
                    {tr("selfTest.cancel")}
                  </button>
                  <button type="button" onClick={runSelfTest}>
                    {tr("selfTest.run")}
                  </button>
                </div>
              </>
            )}
            {selfTest.phase === "running" && (
              <>
                <p style={{ margin: 0 }}>{tr("selfTest.running")}</p>
                <p style={{ margin: "0.5em 0 0", fontSize: "0.8em", opacity: 0.7 }}>{tr("selfTest.rawNote")}</p>
              </>
            )}
            {selfTest.phase === "done" &&
              (() => {
                const v = selfTest.verdict;
                const pct = v.kind === "pass" || v.kind === "mismatch" ? Math.round(Math.max(0, v.r) * 100) : 0;
                const titleKey =
                  v.kind === "pass"
                    ? "passTitle"
                    : v.kind === "fail"
                      ? "failTitle"
                      : v.kind === "mismatch"
                        ? "mismatchTitle"
                        : v.kind === "nosignal"
                          ? "nosignalTitle"
                          : "inconclusiveTitle";
                const color = v.kind === "pass" ? "#16a34a" : v.kind === "fail" ? "#c0392b" : "#b8860b";
                return (
                  <>
                    <p style={{ marginTop: 0, color, fontWeight: 600 }}>{tr(`selfTest.${titleKey}`)}</p>
                    <p style={{ fontSize: "0.9em", whiteSpace: "pre-line" }}>{tr(`selfTest.${v.kind}`, { r: pct })}</p>
                    {(v.kind === "pass" || v.kind === "mismatch") && (v.deviceColorDb ?? 0) > DEVICE_COLOR_DB && (
                      <p style={{ fontSize: "0.85em", color: "#b8860b", marginTop: "-0.3em" }}>
                        {tr("selfTest.deviceColor", { db: (v.deviceColorDb ?? 0).toFixed(1) })}
                      </p>
                    )}
                    <div className="row" style={{ justifyContent: "flex-end", gap: "0.5em" }}>
                      {v.kind !== "pass" && (
                        <button type="button" onClick={() => setSelfTest({ phase: "warn" })}>
                          {tr("selfTest.retry")}
                        </button>
                      )}
                      <button type="button" onClick={() => setSelfTest(null)}>
                        {tr("selfTest.close")}
                      </button>
                    </div>
                  </>
                );
              })()}
          </div>
        </div>
      )}

      {foreignReview && (
        <div
          onClick={() => setForeignReview(null)}
          style={{ position: "fixed", inset: 0, background: "#0006", display: "flex", alignItems: "center", justifyContent: "center", zIndex: 10 }}
        >
          <div className="modal-card" onClick={(e) => e.stopPropagation()}>
            {foreignReview === "review" ? (
              <>
                <p style={{ marginTop: 0, fontWeight: 600 }}>{tr("foreignConfig.reviewTitle")}</p>
                <p style={{ fontSize: "0.9em" }}>{tr("foreignConfig.reviewBody")}</p>
                <pre
                  style={{
                    maxHeight: "9em",
                    overflow: "auto",
                    fontSize: "0.8em",
                    background: "color-mix(in srgb, var(--fg) 8%, transparent)",
                    padding: "0.5em 0.7em",
                    borderRadius: 5,
                    whiteSpace: "pre-wrap",
                    margin: "0.5em 0",
                  }}
                >
                  {foreignConfig?.join("\n")}
                </pre>
                <div className="row" style={{ justifyContent: "flex-end", gap: "0.5em" }}>
                  <button type="button" onClick={() => setForeignReview(null)}>
                    {tr("foreignConfig.cancel")}
                  </button>
                  <button type="button" onClick={commentOutForeign}>
                    {tr("foreignConfig.commentOut")}
                  </button>
                </div>
              </>
            ) : (
              <>
                <p style={{ marginTop: 0, color: "#16a34a", fontWeight: 600 }}>{tr("foreignConfig.doneTitle")}</p>
                <p style={{ fontSize: "0.9em" }}>{tr("foreignConfig.doneBody")}</p>
                <div className="row" style={{ justifyContent: "flex-end", gap: "0.5em" }}>
                  <button type="button" onClick={restoreForeign}>
                    {tr("foreignConfig.undo")}
                  </button>
                  <button type="button" onClick={() => setForeignReview(null)}>
                    {tr("foreignConfig.close")}
                  </button>
                </div>
              </>
            )}
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
              {tr("banner.retry")}
            </button>
          )}
        </div>
      )}

      {/* ---- header: the "set once per session" inputs (§5.1) ---- */}
      <header className="app-header">
        {/* Wordmark: headphone-band-over-EQ-bars glyph (currentColor so it tracks the theme) +
            "Eq" set lighter/dimmer so the EQ part reads as its own token — no gap, no colour. */}
        <span className="brand">
          <svg className="brand-glyph" viewBox="0 0 64 64" role="img" aria-label="CAGEq logo">
            <path d="M13 36 C13 1 51 1 51 36" fill="none" stroke="currentColor" strokeOpacity={0.55} strokeWidth={2.4} strokeLinecap="round" />
            <rect x="9.5" y="31" width="7" height="17" rx="3.5" fill="currentColor" fillOpacity={0.55} />
            <rect x="47.5" y="31" width="7" height="17" rx="3.5" fill="currentColor" fillOpacity={0.55} />
            <g fill="currentColor">
              <rect x="19" y="26" width="3.5" height="21" rx="1.75" />
              <rect x="26.5" y="21" width="3.5" height="37" rx="1.75" />
              <rect x="41.5" y="24" width="3.5" height="23" rx="1.75" />
            </g>
            <rect x="34" y="32" width="3.5" height="26" rx="1.75" fill="#f4b73f" />
          </svg>
          <h1>
            CAG<span className="wordmark-eq">Eq</span>
          </h1>
        </span>
        {!loading && (
          <>
            <span className="row" style={{ gap: "0.4em" }}>
              <label htmlFor="device-select" style={{ fontSize: "0.85em", opacity: 0.75 }}>
                {tr("header.output")}
              </label>
              {devices.length === 0 ? (
                <span className="dev-none" title={tr("header.noDeviceTitle")}>
                  <span aria-hidden>⚠</span> {tr("header.noDevice")}
                </span>
              ) : (
                <>
                  <select id="device-select" value={deviceId} onChange={(e) => changeDevice(e.currentTarget.value)}>
                    {devices.map((d) => (
                      <option key={d.id} value={d.id}>
                        {d.name}
                        {d.eqapo_enabled ? "" : tr("header.apoNotInstalledOption")}
                      </option>
                    ))}
                  </select>
                  {/* Rate readout doubles as the deep-link button — click the format to change it
                      in Windows Sound settings (one control, saves header width). */}
                  <button
                    type="button"
                    className="dev-settings"
                    onClick={openOutputSettings}
                    title={tr("header.soundSettings")}
                    aria-label={tr("header.soundSettings")}
                  >
                    {sampleRate != null && <span className="dev-rate">{fmtRate(sampleRate)}</span>}
                    <span className="dev-gear" aria-hidden>
                      ⚙
                    </span>
                  </button>
                </>
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
              <label style={{ fontSize: "0.85em", opacity: 0.75 }}>{tr("header.headphone")}</label>
              <input
                list="model-list"
                value={query}
                onChange={(e) => onModelInput(e.currentTarget.value)}
                placeholder={tr("header.searchPlaceholder", { count: byModel.size })}
                size={1}
                style={{ width: "13em", minWidth: 0 }}
                disabled={dryActive}
              />
              <datalist id="model-list">
                {modelMatches.map((name) => (
                  <option key={name} value={name} />
                ))}
              </datalist>
              <label style={{ fontSize: "0.85em", opacity: 0.75 }}>{tr("header.measurement")}</label>
              <select
                value={measurementPath}
                onChange={(e) => setMeasurementPath(e.currentTarget.value)}
                disabled={dryActive || measurements.length === 0}
                title={tr("header.measurementTitle")}
              >
                {measurements.length === 0 ? (
                  <option value="">{tr("header.pickModel")}</option>
                ) : (
                  measurements.map((m) => (
                    <option key={m.path} value={m.path}>
                      {tr("header.measBy", { source: m.source })}
                      {m.rig ? tr("header.measOnRig", { rig: m.rig }) : ""}
                    </option>
                  ))
                )}
              </select>
            </form>
          </>
        )}
        <select
          className="lang-select"
          value={i18n.language.startsWith("de") ? "de" : "en"}
          onChange={(e) => setLang(e.currentTarget.value as LangCode)}
          title={tr("lang.label")}
          aria-label={tr("lang.label")}
        >
          {LANGS.map((l) => (
            <option key={l.code} value={l.code} title={l.label}>
              {l.code.toUpperCase()}
            </option>
          ))}
        </select>
        <button
          type="button"
          className="theme-toggle"
          onClick={cycleTheme}
          title={tr("theme.title", { mode: tr(`theme.${theme}`) })}
          aria-label={tr("theme.aria", { mode: tr(`theme.${theme}`) })}
        >
          {/* Per-glyph sizing: ⏾ renders noticeably larger than ◐/☀ in most fonts, so scale it back
              to match the optical size of the other two. */}
          <span className="theme-glyph" style={theme === "dark" ? { fontSize: "0.8em" } : undefined}>
            {theme === "auto" ? "◐" : theme === "light" ? "☀" : "⏾"}
          </span>
        </button>
      </header>

      {!loading && resumeDeviceMissing && (
        <p style={{ color: "#b8860b", fontSize: "0.85em", margin: "0 0 0.8em" }}>
          ⚠ {tr("app.deviceUnavailable", { device: selectedDevice?.name ?? "" })}
        </p>
      )}

      {!loading && selectedDevice && !selectedDevice.eqapo_enabled && (
        <p style={{ color: "#b8860b", fontSize: "0.85em", margin: "0 0 0.8em" }}>
          <Trans i18nKey="app.apoWarning" components={[<em />]} />
        </p>
      )}

      {/* Finding #1: passive (never modal) notice when foreign config.txt filters stack on top of
          CAGEq — only once a correction is applied, and dismissable per session. */}
      {!loading && result && foreignConfig && foreignConfig.length > 0 && !foreignDismissed && (
        <p style={{ color: "#b8860b", fontSize: "0.85em", margin: "0 0 0.8em", display: "flex", alignItems: "center", gap: "0.5em", flexWrap: "wrap" }}>
          <span>⚠ {tr("foreignConfig.notice", { count: foreignConfig.length })}</span>
          <button type="button" onClick={() => setForeignReview("review")} style={{ fontSize: "0.85em" }}>
            {tr("foreignConfig.review")}
          </button>
          <button type="button" onClick={() => setForeignDismissed(true)} style={{ fontSize: "0.85em" }}>
            {tr("foreignConfig.dismiss")}
          </button>
        </p>
      )}

      {/* The cold-start wait is the Python DSP sidecar: after a reboot its numpy/scipy bundle
          is read cold from disk (a couple of seconds; the OS file cache makes repeat launches
          fast) plus the one-time import — I/O-bound, not the catalogue (a 17 ms read). */}
      {loading && <p>{tr("app.loading")}</p>}

      <div className="app-main">
        {/* ================= LEFT: target + chart + bands ================= */}
        <section>
          {!loading && (
            <div className="panel">
              <div className="row" style={{ justifyContent: "space-between", alignItems: "baseline", marginBottom: "0.35rem" }}>
                <h2 style={{ margin: 0 }}>{tr("correction.title")}</h2>
                {/* Kept mounted (space reserved) but hidden on Dry — the impulse view is meaningless
                    for Dry, but removing the toggle collapsed the header row and jumped the layout. */}
                {result && (
                  <div
                    className="chart-view"
                    style={{ margin: 0, visibility: dryActive ? "hidden" : "visible" }}
                    role="group"
                    aria-label={tr("correction.domainAria")}
                    aria-hidden={dryActive || undefined}
                  >
                    {(
                      [
                        ["freq", tr("correction.frequency"), tr("correction.frequencyTitle")],
                        ["time", tr("correction.time"), tr("correction.timeTitle")],
                        ["monitor", tr("correction.monitor"), tr("correction.monitorTitle")],
                        ["scope", tr("correction.scope"), tr("correction.scopeTitle")],
                      ] as const
                    ).map(([id, label, title]) => (
                      <button
                        key={id}
                        type="button"
                        className={chartView === id ? "on" : ""}
                        title={title}
                        aria-pressed={chartView === id}
                        onClick={() => setChartView(id)}
                      >
                        {label}
                      </button>
                    ))}
                  </div>
                )}
              </div>
              <div className="row" style={{ gap: "0.5em" }}>
                <label style={{ fontSize: "0.85em", opacity: 0.75 }}>{tr("correction.target")}</label>
                <select
                  value={targetPath}
                  onChange={(e) => setTargetPath(e.currentTarget.value)}
                  disabled={dryActive}
                  style={{ minWidth: 0, maxWidth: "28em" }}
                >
                  {targets.map((t) => (
                    <option key={t.path} value={t.path}>
                      {t.name}
                    </option>
                  ))}
                </select>
                <button type="button" onClick={() => apply()} disabled={applying || dryActive} style={{ marginLeft: "auto" }}>
                  {applying ? tr("correction.fitting") : dryActive ? tr("correction.applyDry") : tr("correction.apply", { slot: slotLabel(activeSlot) })}
                </button>
              </div>

              {result && (
                <>
                  {/* The Frequency/Time (domain) view swap lives in the panel header row
                      (above). Phase rides the frequency chart's secondary axis (legend). */}
                  <div className="chart-row">
                  <div className="chart-wrap" ref={chartWrapRef}>
                    {scopeView ? (
                      <div className="scope-row">
                        <TimeScope height={215} />
                        <Vectorscope height={215} onPopOut={openScopeWindow} />
                      </div>
                    ) : impulseView && !dryActive ? (
                      <ImpulseChart bands={result.filters} color={SLOT_COLOR[activeSlot]} height={215} legendHost={legendHost} />
                    ) : (
                      <EqChart
                        // Monitor view strips every curve/marker/ref/phase so only the spectrum shows.
                        series={monitorView ? [] : chartSeries}
                        markers={monitorView ? [] : chartMarkers}
                        refs={monitorView ? [] : chartRefs}
                        phase={monitorView ? undefined : chartPhase}
                        spectrumRef={spectrumRef}
                        eqBands={dryActive || selfTest?.phase === "running" ? undefined : result.filters}
                        legendHost={legendHost}
                        minSpan={comparisonSpan}
                        height={215}
                        nodes={{
                          bands: activeBands,
                          color: STAGE_COLOR[activeStage],
                          // Clear the editable drag handles while the read-only AutoEq stage is shown
                          // (same as Dry) or in the clean monitor view — otherwise the last editable
                          // stage's nodes linger on top.
                          disabled: dryActive || autoEqView || monitorView,
                          onChange: (i, patch) => updateFilter(i, patch, 70),
                          onDragEnd: () => {
                            requestApply(0);
                            commitHistory(); // seal the whole chart drag as one undo entry
                          },
                          onAdd: addFilterAt,
                          onRemove: removeFilter,
                          highlightIdx: newBand?.stage === activeStage ? newBand.idx : undefined,
                          hoverIdx: hoverBand,
                          onHover: setHoverBand,
                        }}
                      />
                    )}
                    {/* Preamp is a property of the correction, not the live signal — hide it on the
                        scope (which shows the stereo image, not a level). */}
                    {!scopeView && (
                      <div
                        className="chart-preamp"
                        title={loudness?.mode === "FinalVolume" ? tr("correction.preampTitleMax") : tr("correction.preampTitleMatched")}
                      >
                        {tr("correction.preamp")} <b>{result.preamp_db.toFixed(1)} dB</b>
                        <span className="chart-preamp-mode">{loudness?.mode === "FinalVolume" ? tr("correction.preampMax") : tr("correction.preampMatched")}</span>
                      </div>
                    )}
                  </div>
                    {/* §5.3c post-EQ meters beside the chart (loopback, post-EQ) — always on. */}
                    <div className="meter-col">
                      <Meter deviceId={deviceId} plotBox={plotBox} onSampleRate={setSampleRate} />
                    </div>
                  </div>
                  {/* Full-width host for the chart legend (portaled from the chart) — spans under
                      the meters too, so long/localized toggle labels have room. */}
                  <div ref={setLegendHost} className="chart-legend-host" />
                  {result.clipping_warning && (
                    <p style={{ color: "#b8860b", fontSize: "0.8em", margin: "0.2em 0 0" }}>
                      {tr("correction.clipping")}
                      {loudness?.mode === "Comparison" && headroomPregain != null && headroomPregain < loudness.base_pregain_db && (
                        <button
                          type="button"
                          onClick={addHeadroom}
                          style={{ marginLeft: "0.5em", fontSize: "0.9em", padding: "0.1em 0.5em" }}
                          title={tr("correction.addHeadroomTitle")}
                        >
                          {tr("correction.addHeadroom", { db: headroomPregain })}
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
              <div className="tg-panel-head">
                <h2>{tr("bands.title")}</h2>
                <div className="tg-history" role="group" aria-label={tr("bands.historyAria")}>
                  <button
                    type="button"
                    className="tg-hist-btn"
                    onClick={undo}
                    disabled={dryActive || undoStack.length === 0}
                    title={tr("bands.undoTitle")}
                    aria-label={tr("bands.undoAria")}
                  >
                    ↶
                  </button>
                  <button
                    type="button"
                    className="tg-hist-btn"
                    onClick={redo}
                    disabled={dryActive || redoStack.length === 0}
                    title={tr("bands.redoTitle")}
                    aria-label={tr("bands.redoAria")}
                  >
                    ↷
                  </button>
                </div>
              </div>
              {dryActive ? (
                <p className="tg-empty">{tr("bands.dryNotice")}</p>
              ) : (
                <>
                  {/* Stage selector (segmented): the name button picks the stage to edit; the
                      power icon toggles the whole stage on/off. Active = tinted in the stage colour.
                      A trailing read-only "AutoEq" segment (no power) shows the automatic fit. */}
                  <div className="stage-tabs" role="tablist" aria-label={tr("bands.stagesAria")}>
                    {STAGE_ORDER.map((id) => {
                      const st = stages[id];
                      const isActive = id === activeStage && !autoEqView;
                      const count = st.bands.filter((b) => b.enabled !== false).length;
                      const color = STAGE_COLOR[id];
                      return (
                        <div
                          key={id}
                          className={`stage-seg${isActive ? " active" : ""}${st.enabled ? "" : " off"}`}
                          style={{ "--stage": color } as CSSProperties}
                        >
                          <button
                            type="button"
                            role="tab"
                            aria-selected={isActive}
                            className="stage-seg-select"
                            title={stageHint(id)}
                            onClick={() => {
                              setActiveStage(id);
                              setAutoEqView(false);
                            }}
                          >
                            {stageLabel(id)}
                            {count > 0 && <span className="stage-count">{count}</span>}
                          </button>
                          <button
                            type="button"
                            className="stage-seg-power"
                            role="switch"
                            aria-checked={st.enabled}
                            title={st.enabled ? tr("stages.disable", { stage: stageLabel(id) }) : tr("stages.enable", { stage: stageLabel(id) })}
                            aria-label={st.enabled ? tr("stages.disable", { stage: stageLabel(id) }) : tr("stages.enable", { stage: stageLabel(id) })}
                            onClick={() => toggleStage(id)}
                          >
                            <PowerGlyph />
                          </button>
                        </div>
                      );
                    })}
                    {autoEqCount > 0 && (
                      <div
                        className={`stage-seg stage-seg-ro${autoEqView ? " active" : ""}`}
                        style={{ "--stage": AUTOEQ_COLOR } as CSSProperties}
                      >
                        <button
                          type="button"
                          role="tab"
                          aria-selected={autoEqView}
                          className="stage-seg-select"
                          title={tr("stages.autoeq.hint")}
                          onClick={() => setAutoEqView(true)}
                        >
                          {tr("stages.autoeq.label")}
                          <span className="stage-count">{autoEqCount}</span>
                        </button>
                      </div>
                    )}
                  </div>

                  {autoEqView ? (
                    <>
                      <p className="stage-hint">{tr("stages.autoeq.hint")}</p>
                      <ToneGrid filters={autoEqBands} readOnly accent={AUTOEQ_COLOR} onInput={() => {}} onCommit={() => {}} onAdd={() => {}} onRemove={() => {}} />
                      <p className="tg-hint">{tr("bands.autoeqReadonly")}</p>
                    </>
                  ) : (
                    <>
                      <p className="stage-hint">
                        {stageHint(activeStage)}
                        {!stages[activeStage].enabled && <b>{tr("stages.disabledSuffix")}</b>}
                      </p>

                      <ToneGrid
                        filters={activeBands}
                        disabled={dryActive}
                        accent={STAGE_COLOR[activeStage]}
                        focusIndex={newBand?.stage === activeStage && newBand.focusGrid ? newBand.idx : null}
                        focusNonce={newBand?.nonce}
                        hoverIndex={hoverBand}
                        onHover={setHoverBand}
                        soloIndex={solo?.stage === activeStage ? solo.idx : null}
                        onSolo={toggleSolo}
                        isolateIndex={isolate?.stage === activeStage ? isolate.idx : null}
                        onIsolate={toggleIsolate}
                        onInput={(i, patch) => updateFilter(i, patch, 70)}
                        onCommit={(i, patch) => updateFilter(i, patch, 0)}
                        onAdd={addFilter}
                        onRemove={removeFilter}
                      />
                      {/* Always rendered (with reserved height) so switching to an empty stage
                          doesn't shrink the panel; the text just adapts to empty vs populated. */}
                      <p className="tg-hint">{activeBands.length > 0 ? tr("bands.hint") : tr("bands.emptyHint")}</p>
                    </>
                  )}
                </>
              )}
            </div>
          )}
        </section>

        {/* ================= RIGHT: slots + loudness + presets ================= */}
        <aside>
          {!loading && (
            <div className="panel">
              <h2>{tr("compare.title")}</h2>
              <div className="row" style={{ gap: "0.4em" }}>
                {SLOT_ORDER.map((s) => {
                  const active = s === activeSlot;
                  const populated = s === "Dry" || slotInputs[s] !== null;
                  const key = s === "A" ? "A" : s === "B" ? "S" : "D";
                  return (
                    <button
                      key={s}
                      type="button"
                      className={`slot-chip${active ? " active" : ""}${populated || active ? "" : " empty"}`}
                      onClick={() => switchSlot(s)}
                      title={tr("compare.slotTitle", { slot: slotLabel(s), key }) + (populated ? "" : tr("compare.slotEmptySuffix"))}
                      style={{ "--slot": SLOT_COLOR[s] } as CSSProperties}
                    >
                      {slotLabel(s)} <kbd>{key}</kbd>
                    </button>
                  );
                })}
              </div>
              {(slotPreset.A || slotPreset.B) && (
                <div className="slot-loaded">
                  {(["A", "B"] as const).map((s) => {
                    const ref = slotPreset[s];
                    if (!ref) return null;
                    const dirty = slotDirty(s);
                    const ver = presetVerLabel(ref);
                    return (
                      <span
                        key={s}
                        className={`sl-item${s === activeSlot ? " active" : ""}`}
                        style={{ "--slot": SLOT_COLOR[s], gridColumn: s === "A" ? 1 : 2 } as CSSProperties}
                        title={tr(dirty ? "compare.loadedDirtyTitle" : "compare.loadedTitle", { slot: slotLabel(s), name: ref.name, ver: ver ?? "" })}
                      >
                        <span className="sl-name">{ref.name}</span>
                        {ver && <span className="sl-ver">{ver}</span>}
                        {dirty && <span className="sl-dirty" aria-label={tr("compare.dirtyAria")}>●</span>}
                      </span>
                    );
                  })}
                </div>
              )}
              <div className="row" style={{ gap: "0.4em", marginTop: "0.4em" }}>
                <span style={{ fontSize: "0.75em", opacity: 0.6 }}>{tr("compare.copy")}</span>
                <button type="button" onClick={() => copySlot("A", "B")} disabled={!slotInputs.A} style={{ fontSize: "0.8em" }}>
                  A→B
                </button>
                <button type="button" onClick={() => copySlot("B", "A")} disabled={!slotInputs.B} style={{ fontSize: "0.8em" }}>
                  B→A
                </button>
                <button
                  type="button"
                  onClick={() => setSelfTest({ phase: "warn" })}
                  disabled={dryActive || !result}
                  title={tr("selfTest.buttonTitle")}
                  style={{ fontSize: "0.8em", marginLeft: "auto" }}
                >
                  {tr("selfTest.button")}
                </button>
              </div>
              <p style={{ fontSize: "0.75em", opacity: 0.6, margin: "0.5em 0 0" }}>{tr("compare.shortcuts")}</p>
              {loudness?.mode === "FinalVolume" && (slotInputs.A !== null || slotInputs.B !== null) && (
                <p style={{ color: "#b8860b", fontSize: "0.78em", margin: "0.4em 0 0" }}>
                  <Trans i18nKey="compare.finalWarning" components={[<kbd />]} />
                </p>
              )}
            </div>
          )}

          {loudness && (
            <div className="panel">
              <h2>{tr("loudness.title")}</h2>
              {/* Segmented mode selector, lit like the stage/slot chips: Comparison in the accent,
                  Final volume in amber — the "hot" (loudest-safe) mode, which is confirm-gated. */}
              <div className="ld-modes" role="radiogroup" aria-label={tr("loudness.modeAria")}>
                <button
                  type="button"
                  role="radio"
                  aria-checked={loudness.mode === "Comparison"}
                  className={`ld-mode${loudness.mode === "Comparison" ? " active" : ""}`}
                  style={{ "--md": "#3b82f6" } as CSSProperties}
                  onClick={() => requestLoudness({ ...loudness, mode: "Comparison" })}
                >
                  <span className="ld-mode-t">{tr("loudness.comparison")}</span>
                  <span className="ld-mode-s">{tr("loudness.comparisonSub")}</span>
                </button>
                <button
                  type="button"
                  role="radio"
                  aria-checked={loudness.mode === "FinalVolume"}
                  className={`ld-mode${loudness.mode === "FinalVolume" ? " active" : ""}`}
                  style={{ "--md": "#daa520" } as CSSProperties}
                  onClick={() => requestLoudness({ ...loudness, mode: "FinalVolume" })}
                >
                  <span className="ld-mode-t">{tr("loudness.final")}</span>
                  <span className="ld-mode-s">{tr("loudness.finalSub")}</span>
                </button>
              </div>
              <label className="row" style={{ opacity: loudness.mode === "Comparison" ? 1 : 0.4, fontSize: "0.85em" }}>
                {tr("loudness.basePregain")}
                <PreampField
                  value={loudness.base_pregain_db}
                  disabled={loudness.mode !== "Comparison"}
                  ariaLabel={tr("loudness.basePregain")}
                  onCommit={commitBasePregain}
                />
                dB
              </label>
              <p style={{ fontSize: "0.75em", opacity: 0.7, margin: "0.5em 0 0" }}>
                {loudness.mode === "Comparison" ? tr("loudness.comparisonDesc") : tr("loudness.finalDesc")}
              </p>
              <label style={{ fontSize: "0.75em", opacity: 0.8, display: "flex", alignItems: "center", gap: "0.4em", marginTop: "0.5em" }}>
                <input
                  type="checkbox"
                  checked={confirmFinalVolume}
                  onChange={(e) => toggleConfirmFinalVolume(e.currentTarget.checked)}
                  style={{ flex: "none", margin: 0 }}
                />
                {tr("loudness.confirmToggle")}
              </label>
            </div>
          )}

          {!loading && (
            <div className="panel" style={{ opacity: dryActive ? 0.5 : 1 }}>
              <div className="row" style={{ justifyContent: "space-between", alignItems: "baseline" }}>
                <h2 style={{ margin: 0 }}>{tr("presets.title")}</h2>
                <button
                  type="button"
                  className="pl-save"
                  disabled={dryActive}
                  onClick={() =>
                    setSaveForm(saveForm ? null : { kind: measurementPath ? "preset" : "template", name: "", error: false })
                  }
                  title={tr("presets.saveTitle")}
                >
                  <span aria-hidden>💾</span> {tr("presets.save")}
                </button>
              </div>

              {saveForm && (
                <div className="pl-saveform">
                  <div className="pl-toggle">
                    <button type="button" className={saveForm.kind === "template" ? "on" : ""} onClick={() => setSaveForm({ ...saveForm, kind: "template" })}>
                      {tr("presets.filterTemplate")}
                    </button>
                    <button
                      type="button"
                      className={saveForm.kind === "preset" ? "on" : ""}
                      disabled={!measurementPath}
                      title={measurementPath ? undefined : tr("presets.fullPresetDisabledTitle")}
                      onClick={() => setSaveForm({ ...saveForm, kind: "preset" })}
                    >
                      {tr("presets.fullPreset")}
                    </button>
                  </div>
                  <p className="pl-hint">
                    {saveForm.kind === "preset"
                      ? tr("presets.presetHint")
                      : tr("presets.templateHint", { stage: stageLabel(activeStage) })}
                  </p>
                  <div className="row" style={{ gap: "0.3em" }}>
                    <input
                      type="text"
                      placeholder={tr("presets.namePlaceholder")}
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
                      {tr("presets.save")}
                    </button>
                    <button type="button" onClick={() => setSaveForm(null)}>
                      {tr("presets.cancel")}
                    </button>
                  </div>
                </div>
              )}

              <h3 className="pl-group">
                {tr("presets.filterTemplates")} <span>{showAllStages ? tr("presets.scopeAllStages") : stageLabel(activeStage)}</span>
                <button type="button" className="pl-showall" onClick={() => setShowAllStages((v) => !v)}>
                  {showAllStages ? tr("presets.activeStage") : tr("presets.showAll")}
                </button>
              </h3>
              <div className="pl-scroll">
                {showCurated && (
                  <div className="row pl-curated">
                    {CURATED_TEMPLATES.map((t) => (
                      <button key={t.id} type="button" disabled={dryActive} onClick={() => loadTemplate(t)}>
                        {tr(`tonePresets.${TONE_PRESET_KEY[t.name]}`)}
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
                          {stageLabel(t.stage)}
                        </span>
                      )}
                      {renaming?.kind === "template" && renaming.id === t.id ? (
                        <input
                          className="pl-rename"
                          autoFocus
                          value={renaming.name}
                          onChange={(e) => setRenaming({ ...renaming, name: e.currentTarget.value })}
                          onBlur={commitRename}
                          onKeyDown={(e) => {
                            if (e.key === "Enter") commitRename();
                            else if (e.key === "Escape") setRenaming(null);
                          }}
                        />
                      ) : (
                        <span className="pl-name" title={t.name} onDoubleClick={() => setRenaming({ kind: "template", id: t.id, name: t.name })}>
                          {t.name}
                        </span>
                      )}
                      <button type="button" disabled={dryActive} onClick={() => loadTemplate(t)}>
                        {tr("presets.load")}
                      </button>
                      <button
                        type="button"
                        className="pl-ren"
                        title={tr("presets.renameTitle")}
                        aria-label={tr("presets.renameAria", { name: t.name })}
                        onClick={() => setRenaming({ kind: "template", id: t.id, name: t.name })}
                      >
                        ✎
                      </button>
                      <button
                        type="button"
                        className="pl-upd"
                        title={tr("presets.updateTemplateTitle", { stage: stageLabel(t.stage) })}
                        disabled={dryActive}
                        onClick={() => updateTemplate(t)}
                      >
                        💾
                      </button>
                      <button type="button" className="pl-del" title={tr("presets.deleteTitle")} onClick={() => deleteTemplate(t)}>
                        🗑
                      </button>
                    </li>
                  ))}
                </ul>
                )}
              </div>

              <h3 className="pl-group">
                {tr("presets.presetsGroup")} <span>{tr("presets.presetsScope")}</span>
              </h3>
              <div className="pl-scroll">
                {library.presets.length > 0 ? (
                  <ul className="pl-list">
                    {library.presets.map((p) => (
                      <li key={p.id} className="pl-preset">
                        <div className="pl-item">
                          {renaming?.kind === "preset" && renaming.id === p.id ? (
                            <input
                              className="pl-rename"
                              autoFocus
                              value={renaming.name}
                              onChange={(e) => setRenaming({ ...renaming, name: e.currentTarget.value })}
                              onBlur={commitRename}
                              onKeyDown={(e) => {
                                if (e.key === "Enter") commitRename();
                                else if (e.key === "Escape") setRenaming(null);
                              }}
                            />
                          ) : (
                            <span
                              className="pl-name"
                              title={`${p.name} — ${p.model || tr("presets.noMeasurement")}`}
                              onDoubleClick={() => setRenaming({ kind: "preset", id: p.id, name: p.name })}
                            >
                              {p.name}
                            </span>
                          )}
                          <button type="button" disabled={dryActive} onClick={() => loadPreset(p, { id: p.id, name: p.name, at: "head" })}>
                            {tr("presets.load")}
                          </button>
                          <button
                            type="button"
                            className={`pl-ver-toggle${expandedPreset === p.id ? " open" : ""}`}
                            title={tr("presets.versionsTitle")}
                            aria-expanded={expandedPreset === p.id}
                            onClick={() => setExpandedPreset((cur) => (cur === p.id ? null : p.id))}
                          >
                            v{(p.versions?.length ?? 0) + 1}
                          </button>
                          <button
                            type="button"
                            className="pl-ren"
                            title={tr("presets.renameTitle")}
                            aria-label={tr("presets.renameAria", { name: p.name })}
                            onClick={() => setRenaming({ kind: "preset", id: p.id, name: p.name })}
                          >
                            ✎
                          </button>
                          <button
                            type="button"
                            className="pl-upd"
                            title={measurementPath ? tr("presets.savePresetTitle") : tr("presets.updatePresetDisabledTitle")}
                            disabled={dryActive || !measurementPath}
                            onClick={() => setPresetSave(p)}
                          >
                            💾
                          </button>
                          <button type="button" className="pl-del" title={tr("presets.deleteTitle")} onClick={() => deletePreset(p)}>
                            🗑
                          </button>
                        </div>
                        {expandedPreset === p.id && (
                          <div className="pl-versions">
                            <div className="pl-versions-head">
                              <span>{tr("presets.versions")}</span>
                              {/* Deletes the *newest* version (the head), reverting to the version
                                  below it — the head isn't itself an entry in `p.versions`, so it
                                  has no row/trash-icon of its own in the list below. Only offered
                                  when there's a version to fall back to; with none, deleting the
                                  head is deleting the whole preset (the row's own trash button). */}
                              {(p.versions?.length ?? 0) > 0 && (
                                <button
                                  type="button"
                                  className="pl-del pl-del-head"
                                  disabled={dryActive}
                                  title={tr("presets.deleteHeadVersionTitle", { ver: `v${(p.versions?.length ?? 0) + 1}` })}
                                  onClick={() => deleteHeadVersion(p)}
                                >
                                  🗑 v{(p.versions?.length ?? 0) + 1}
                                </button>
                              )}
                            </div>
                            {(p.versions?.length ?? 0) === 0 ? (
                              <p className="pl-versions-empty">{tr("presets.noVersions")}</p>
                            ) : (
                              <ul className="pl-versions-list">
                                {(p.versions ?? [])
                                  .map((v, i) => ({ v, i }))
                                  .reverse()
                                  .map(({ v, i }) => (
                                    <li key={i}>
                                      <span className="pl-ver-when">
                                        v{i + 1} · {fmtWhen(v.at)}
                                      </span>
                                      <button type="button" disabled={dryActive} onClick={() => loadPreset(v, { id: p.id, name: p.name, at: v.at })}>
                                        {tr("presets.load")}
                                      </button>
                                      <button
                                        type="button"
                                        className="pl-del"
                                        title={tr("presets.deleteVersionTitle")}
                                        onClick={() => deleteVersion(p, i)}
                                      >
                                        🗑
                                      </button>
                                    </li>
                                  ))}
                              </ul>
                            )}
                          </div>
                        )}
                      </li>
                    ))}
                  </ul>
                ) : (
                  <p className="pl-empty">{tr("presets.emptyPresets")}</p>
                )}
              </div>
            </div>
          )}
        </aside>
      </div>

      {error && <p style={{ color: "crimson" }}>{error}</p>}

      <footer className="app-footer">
        {appVersion && (
          <>
            <span className="app-version">CAGEq v{appVersion}</span> ·{" "}
          </>
        )}
        <Trans
          i18nKey="footer.text"
          components={[
            <a
              href="https://github.com/jaakkopasanen/AutoEq"
              onClick={(e) => {
                e.preventDefault();
                void openUrl("https://github.com/jaakkopasanen/AutoEq").catch(() => {});
              }}
            />,
          ]}
        />
      </footer>
    </main>
  );
}

export default App;
