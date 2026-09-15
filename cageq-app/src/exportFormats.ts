/**
 * §8 mobile export: text formats a phone EQ app can import (or a user can type in by hand),
 * following the AutoEq website's own model. Two independent ports of AutoEq's own
 * `FrequencyResponse` methods (`cageq-sidecar/.venv/Lib/site-packages/autoeq/
 * frequency_response.py`), so the output matches what AutoEq's own site would produce for an
 * equivalent curve — the same reasoning `biquad.ts`'s own header gives for mirroring AutoEq's
 * biquad model exactly.
 *
 * Both take a `Band[]` and evaluate {@link composedCurveDb} directly (the exact analytic biquad
 * model, not a discrete measurement) rather than AutoEq's own spline-interpolate-a-coarser-grid
 * step — strictly more accurate here since there's no raw measurement grid to be limited by, and
 * it's the one curve implementation the whole app already trusts (§5.2's own chart, the
 * biquad-crosscheck tests pinning it against Rust/Python).
 */
import { Band, composedCurveDb, FS } from "./biquad";

// AutoEq's own GraphicEQ constants (autoeq/constants.py) — DEFAULT_GRAPHIC_EQ_STEP produces 127
// points from 20 Hz up to ~19871 Hz; PREAMP_HEADROOM is the same 0.2 dB safety margin AutoEq's
// own `eqapo_graphic_eq`/`write_eqapo_parametric_eq` both bake into their normalization.
const GRAPHIC_EQ_STEP = 1.0563;
const PREAMP_HEADROOM = 0.2;

/** The fixed 127-point log frequency grid AutoEq's GraphicEQ format samples at (20 Hz, step
 *  ratio 1.0563, truncated to integer Hz, deduplicated) — computed once, not per export. */
const GRAPHIC_EQ_FREQS: number[] = (() => {
  const n = Math.ceil(Math.log(20000 / 20) / Math.log(GRAPHIC_EQ_STEP));
  const set = new Set<number>();
  for (let i = 0; i < n; i++) set.add(Math.trunc(20 * GRAPHIC_EQ_STEP ** i));
  return Array.from(set).sort((a, b) => a - b);
})();

/** `GraphicEQ: f db; f db; ...` — a dense, exact resample of the composed curve (no band-count
 *  tradeoff at all, unlike {@link parametricEqText}), broadly supported by mobile EQ apps
 *  (Wavelet, PowerAmp, RootlessJamsDSP/ViPER4Android, ...). Port of AutoEq's own
 *  `eqapo_graphic_eq`: normalized so the peak sits `PREAMP_HEADROOM` dB below 0 (this format has
 *  no separate preamp line — the destination app plays it back as-is), and the lowest point is
 *  clamped non-positive so a rising low end can't read as a below-20-Hz bass boost. */
export function graphicEqText(bands: Band[], sampleRate = FS): string {
  const freqs = Float64Array.from(GRAPHIC_EQ_FREQS);
  const raw = composedCurveDb(bands, freqs, sampleRate);
  const peak = Math.max(...raw);
  const shifted = Array.from(raw, (v) => v - (peak + PREAMP_HEADROOM));
  if (shifted[0] > 0) shifted[0] = 0;
  const pairs = GRAPHIC_EQ_FREQS.map((f, i) => `${f} ${shifted[i].toFixed(1)}`).join("; ");
  return `GraphicEQ: ${pairs}`;
}

// AutoEq's own filter-type -> EqAPO code map (`write_eqapo_parametric_eq`), reused verbatim —
// Bandpass/Tilt never appear here (the export fit only ever emits Peaking/LowShelf/HighShelf,
// see fit_export_eq's own doc), so PK is a safe fallback rather than a real branch.
const FILTER_TYPE_CODE: Record<string, string> = { Peaking: "PK", LowShelf: "LSC", HighShelf: "HSC" };

/** `Preamp: X dB` + one `Filter N: ON {PK|LSC|HSC} Fc.. Hz Gain.. dB Q..` line per band — the
 *  EqualizerAPO/AutoEq parametric syntax, broadly paste-compatible with mobile parametric EQs
 *  (Wavelet, Neutron, USB Audio Player Pro, ...). `bands`/`preampDb` are the low-band-count
 *  `fit_export_eq` result, not the slot's full cascade — this is the band-limited export. */
export function parametricEqText(bands: Band[], preampDb: number): string {
  let s = `Preamp: ${preampDb.toFixed(1)} dB\n`;
  bands.forEach((b, i) => {
    const code = FILTER_TYPE_CODE[b.kind] ?? "PK";
    s += `Filter ${i + 1}: ON ${code} Fc ${Math.round(b.freq_hz)} Hz Gain ${b.gain_db.toFixed(1)} dB Q ${b.q.toFixed(2)}\n`;
  });
  return s;
}
