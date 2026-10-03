/**
 * §8 mobile export: text a phone EQ app can import (or a user can type in by hand), following
 * the AutoEq website's own formats.
 *
 * - **Parametric** (`fit_export_eq`, a backend solve): a Peaking/LowShelf/HighShelf `Band[]` in
 *   the EqualizerAPO/AutoEq parametric syntax — a port of AutoEq's `write_eqapo_parametric_eq`.
 * - **Graphic EQ**, no solver — the slot's own composed curve, sampled:
 *   - the dense `GraphicEQ:` curve (AutoEq's 127-point format), exact by construction;
 *   - slider values for a 10-/31-band graphic EQ: the curve's average over each band.
 *
 * The graphic presets used to be a second solve, as AutoEq does it: each slider modelled as a
 * constant-Q peaking filter at an assumed Q (√2 / 4.318), gains fitted so the overlapping bands
 * sum to the curve. Real graphic EQs mostly don't work that way (checked 2026-10): Wavelet runs
 * on Android's `DynamicsProcessing`, an FFT EQ where each band is one flat gain up to its cutoff
 * with no overlap; Poweramp imports a `GraphicEQ` curve by averaging it into its sliders;
 * Android's stock equalizer is 5 bands at Q 0.96; Equalizer APO's `GraphicEQ` interpolates its
 * points. Gains compensating for an overlap those apps don't have are wrong in them, so the
 * export samples the curve instead — exact for the dense format, and the per-band average is
 * what flat-band and averaging apps do with a curve themselves.
 */
import { Band, composedCurveDb, type ResponseModel } from "./biquad";

// AutoEq's own filter-type -> EqAPO code map (`write_eqapo_parametric_eq`), reused verbatim —
// Bandpass/Tilt never appear here (the export only emits Peaking/LowShelf/HighShelf), so PK is a
// safe fallback rather than a real branch.
const FILTER_TYPE_CODE: Record<string, string> = { Peaking: "PK", LowShelf: "LSC", HighShelf: "HSC" };

/** Headroom below 0 dB for a self-contained export (AutoEq's `PREAMP_HEADROOM`, the same 0.2 dB the
 *  parametric fit's preamp uses). */
const PREAMP_HEADROOM_DB = 0.2;

/** `Preamp: X dB` + one `Filter N: ON {PK|LSC|HSC} Fc.. Hz Gain.. dB [Q..]` line per band — the
 *  EqualizerAPO/AutoEq parametric syntax, broadly paste-compatible with parametric EQ apps
 *  (Poweramp, Neutron, USB Audio Player Pro, ...).
 *
 *  `includeQ` drops the trailing `Q..` field entirely when false — AutoEq's own 10-/31-band
 *  downloads omit Q too. Graphic-EQ slider values pass `false`: there Q isn't the export's to set,
 *  each app's sliders have their own band shape. */
export function parametricEqText(bands: Band[], preampDb: number, includeQ = true): string {
  let s = `Preamp: ${preampDb.toFixed(1)} dB\n`;
  bands.forEach((b, i) => {
    const code = FILTER_TYPE_CODE[b.kind] ?? "PK";
    const q = includeQ ? ` Q ${b.q.toFixed(2)}` : "";
    s += `Filter ${i + 1}: ON ${code} Fc ${Math.round(b.freq_hz)} Hz Gain ${b.gain_db.toFixed(1)} dB${q}\n`;
  });
  return s;
}

/** AutoEq's `GraphicEQ` frequency grid (127 points, ~1/12.5 octave), taken verbatim from AutoEq
 *  output so importers see the grid they already know. */
export const GRAPHIC_EQ_FREQS = [
  20, 21, 22, 23, 24, 26, 27, 29, 30, 32, 34, 36, 38, 40, 43, 45, 48, 50, 53, 56, 59, 63, 66, 70, 74, 78, 83, 87, 92, 97, 103, 109, 115, 121, 128,
  136, 143, 151, 160, 169, 178, 188, 199, 210, 222, 235, 248, 262, 277, 292, 309, 326, 345, 364, 385, 406, 429, 453, 479, 506, 534, 565, 596, 630,
  665, 703, 743, 784, 829, 875, 924, 977, 1032, 1090, 1151, 1216, 1284, 1357, 1433, 1514, 1599, 1689, 1784, 1885, 1991, 2103, 2221, 2347, 2479,
  2618, 2766, 2921, 3086, 3260, 3443, 3637, 3842, 4058, 4287, 4528, 4783, 5052, 5337, 5637, 5955, 6290, 6644, 7018, 7414, 7831, 8272, 8738, 9230,
  9749, 10298, 10878, 11490, 12137, 12821, 13543, 14305, 15110, 15961, 16860, 17809, 18812, 19871,
];

/** One graphic-EQ point: a frequency and the gain to set there. */
export type GraphicPoint = { freq_hz: number; gain_db: number };

/** The dense curve: the slot's composed curve at every `GRAPHIC_EQ_FREQS` point, shifted down so
 *  its maximum sits at -0.2 dB — the preamp folded in, as AutoEq's normalised `GraphicEQ` does,
 *  since the format has no preamp line of its own. `model`/`sampleRate`: the curve as heard. */
export function graphicEqCurve(filters: Band[], model: ResponseModel, sampleRate?: number): GraphicPoint[] {
  const db = composedCurveDb(filters, Float64Array.from(GRAPHIC_EQ_FREQS), model, sampleRate);
  const shift = Math.max(...db) + PREAMP_HEADROOM_DB;
  return GRAPHIC_EQ_FREQS.map((f, i) => ({ freq_hz: f, gain_db: db[i] - shift }));
}

/** `GraphicEQ: 20 -1.2; 21 -1.3; …` — Equalizer APO's GraphicEQ syntax, as AutoEq writes it. */
export function graphicEqText(points: GraphicPoint[]): string {
  return `GraphicEQ: ${points.map((p) => `${p.freq_hz} ${p.gain_db.toFixed(1)}`).join("; ")}\n`;
}

export type SliderPreset = "10" | "31";

/** Centre frequencies of AutoEq's 10-/31-band presets (octave from 31.25 Hz / third-octave from
 *  20 Hz), and each band's width in octaves. */
function sliderCentres(preset: SliderPreset): { centres: number[]; widthOct: number } {
  return preset === "10"
    ? { centres: Array.from({ length: 10 }, (_, i) => 31.25 * 2 ** i), widthOct: 1 }
    : { centres: Array.from({ length: 31 }, (_, i) => 20 * 2 ** (i / 3)), widthOct: 1 / 3 };
}

/** Slider values for a 10-/31-band graphic EQ: the composed curve's average (in dB, over log
 *  frequency) across each band's own range — centre ± half its width, clamped to 20 Hz-20 kHz.
 *  Not overlap-compensated (see the module doc). Returned as Peaking bands at the centres so the
 *  parametric text/table code can show them, with Q set to the band's nominal octave width for
 *  display only. `preampDb` keeps the highest slider at -0.2 dB. */
export function sliderValues(filters: Band[], preset: SliderPreset, model: ResponseModel, sampleRate?: number): { bands: Band[]; preampDb: number } {
  const { centres, widthOct } = sliderCentres(preset);
  const SUB = 24; // samples per band: plenty for a smooth curve over at most an octave
  const edges = centres.map((fc) => [Math.max(20, fc * 2 ** (-widthOct / 2)), Math.min(20000, fc * 2 ** (widthOct / 2))]);
  const freqs = new Float64Array(centres.length * SUB);
  edges.forEach(([lo, hi], b) => {
    for (let k = 0; k < SUB; k++) freqs[b * SUB + k] = lo * (hi / lo) ** ((k + 0.5) / SUB);
  });
  const db = composedCurveDb(filters, freqs, model, sampleRate);
  const q = 2 ** (widthOct / 2) / (2 ** widthOct - 1); // nominal Q of an N-octave band (AutoEq's formula)
  const bands: Band[] = centres.map((fc, b) => {
    let sum = 0;
    for (let k = 0; k < SUB; k++) sum += db[b * SUB + k];
    return { kind: "Peaking", freq_hz: fc, gain_db: sum / SUB, q };
  });
  const preampDb = -(Math.max(...bands.map((b) => b.gain_db)) + PREAMP_HEADROOM_DB);
  return { bands, preampDb };
}

/** The curve a flat-band graphic EQ (Wavelet / Android `DynamicsProcessing`) produces from slider
 *  values — each band's gain held constant across its range — for the export preview. Boundaries
 *  sit midway (in log frequency) between neighbouring centres. */
export function flatBandCurve(bands: Band[], freqs: Float64Array): Float64Array {
  const out = new Float64Array(freqs.length);
  for (let i = 0; i < freqs.length; i++) {
    let b = 0;
    while (b < bands.length - 1 && freqs[i] > Math.sqrt(bands[b].freq_hz * bands[b + 1].freq_hz)) b++;
    out[i] = bands[b].gain_db;
  }
  return out;
}

/** Log-frequency linear interpolation of graphic points onto `freqs` (how Equalizer APO reads a
 *  GraphicEQ line), flat beyond the ends — the dense curve's preview. */
export function interpolatePoints(points: GraphicPoint[], freqs: Float64Array): Float64Array {
  const out = new Float64Array(freqs.length);
  let j = 0;
  for (let i = 0; i < freqs.length; i++) {
    const f = freqs[i];
    if (f <= points[0].freq_hz) { out[i] = points[0].gain_db; continue; }
    if (f >= points[points.length - 1].freq_hz) { out[i] = points[points.length - 1].gain_db; continue; }
    while (points[j + 1].freq_hz < f) j++;
    const a = points[j], b = points[j + 1];
    const t = Math.log(f / a.freq_hz) / Math.log(b.freq_hz / a.freq_hz);
    out[i] = a.gain_db + (b.gain_db - a.gain_db) * t;
  }
  return out;
}
