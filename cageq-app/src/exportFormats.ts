/**
 * §8 mobile export: text a phone EQ app can import (or a user can type in by hand), following
 * the AutoEq website's own model. Both the free band-count fit (`fit_export_eq`) and AutoEq's
 * own standard 10-/31-band graphic EQ (`fit_fixed_band_eq`) resolve to a plain Peaking/LowShelf/
 * HighShelf `Band[]` — this file's only job is formatting one of those as the EqualizerAPO/
 * AutoEq parametric syntax, a port of AutoEq's own `write_eqapo_parametric_eq`
 * (`cageq-sidecar/.venv/Lib/site-packages/autoeq/frequency_response.py:199`), which is how
 * AutoEq's own site writes out ALL three of these (free parametric, 10-band, 31-band) — its
 * dense curve-sample `GraphicEQ:` format is a different, unrelated download this app doesn't
 * offer, since the standard 10-/31-band presets cover the "graphic EQ" use case already.
 */
import { Band } from "./biquad";

// AutoEq's own filter-type -> EqAPO code map (`write_eqapo_parametric_eq`), reused verbatim —
// Bandpass/Tilt never appear here (both export fits only ever emit Peaking/LowShelf/HighShelf,
// see their own Python docstrings), so PK is a safe fallback rather than a real branch.
const FILTER_TYPE_CODE: Record<string, string> = { Peaking: "PK", LowShelf: "LSC", HighShelf: "HSC" };

/** `Preamp: X dB` + one `Filter N: ON {PK|LSC|HSC} Fc.. Hz Gain.. dB [Q..]` line per band — the
 *  EqualizerAPO/AutoEq parametric syntax, broadly paste-compatible with mobile parametric EQs
 *  (Wavelet, Neutron, USB Audio Player Pro, ...). `bands`/`preampDb` are a `fit_export_eq` or
 *  `fit_fixed_band_eq` result, not the slot's full cascade — this is always the band-limited
 *  export, never the full cascade directly.
 *
 *  `includeQ` drops the trailing `Q..` field entirely when false — confirmed against AutoEq's
 *  own site: its real 10-/31-band downloads omit Q (it's fixed by the preset, so per-line it'd
 *  just be the same constant repeated, implied rather than stated), while its free parametric
 *  download states Q per line since there it genuinely varies band to band. `fit_export_eq`
 *  (free Fc/Q) should always pass `true`; `fit_fixed_band_eq` (fixed Fc/Q presets) `false`. */
export function parametricEqText(bands: Band[], preampDb: number, includeQ = true): string {
  let s = `Preamp: ${preampDb.toFixed(1)} dB\n`;
  bands.forEach((b, i) => {
    const code = FILTER_TYPE_CODE[b.kind] ?? "PK";
    const q = includeQ ? ` Q ${b.q.toFixed(2)}` : "";
    s += `Filter ${i + 1}: ON ${code} Fc ${Math.round(b.freq_hz)} Hz Gain ${b.gain_db.toFixed(1)} dB${q}\n`;
  });
  return s;
}
